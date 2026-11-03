// Behaviour tests for `FrontendRequest::SetDispatchPaused`'s reason
// requirement — dispatch must never be found paused with no record of what
// paused it or why, so a pause request with a missing or whitespace-only
// `reason` must be rejected outright, not silently accepted with no
// reason on record.

use super::*;
use crate::app::engine_meta;

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

async fn set_dispatch_paused(state: &Arc<ServerState>, paused: bool, reason: Option<&str>) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch(state, &sink);
    engine_meta::handle_set_dispatch_paused(
        ctx,
        FrontendRequest::SetDispatchPaused {
            paused,
            reason: reason.map(str::to_owned),
            scope: None,
        },
    )
    .await;
    sole_response(&sink).await
}

#[tokio::test]
async fn pause_with_no_reason_is_rejected_and_dispatch_stays_unpaused() {
    let (server_state, _dir) = test_server_state();

    let response = set_dispatch_paused(&server_state, true, None).await;

    assert!(
        matches!(response, FrontendEvent::WorkError { .. }),
        "a reason-less pause must be rejected, got {response:?}",
    );
    assert!(
        !server_state.execution_coordinator.is_dispatch_paused(),
        "a rejected pause must not leave dispatch paused",
    );
}

#[tokio::test]
async fn pause_with_whitespace_only_reason_is_rejected_and_dispatch_stays_unpaused() {
    let (server_state, _dir) = test_server_state();

    let response = set_dispatch_paused(&server_state, true, Some("   ")).await;

    assert!(
        matches!(response, FrontendEvent::WorkError { .. }),
        "a whitespace-only reason must be rejected same as no reason at all, got {response:?}",
    );
    assert!(
        !server_state.execution_coordinator.is_dispatch_paused(),
        "a rejected pause must not leave dispatch paused",
    );
}

// ── pause scope: what the pause holds, chosen by the operator ─────────────

use crate::app::handler_helpers::{
    METADATA_KEY_DISPATCH_PAUSE_ORIGIN, METADATA_KEY_DISPATCH_PAUSE_SCOPE, METADATA_KEY_DISPATCH_PAUSED,
    restore_persisted_dispatch_pause,
};
use boss_engine_dispatch::coordinator::DispatchPauseOrigin;
use boss_protocol::DispatchPauseScope;

async fn pause_with_scope(state: &Arc<ServerState>, scope: Option<DispatchPauseScope>) -> FrontendEvent {
    let sink = make_session_sink();
    engine_meta::handle_set_dispatch_paused(
        dispatch(state, &sink),
        FrontendRequest::SetDispatchPaused {
            paused: true,
            reason: Some("spawn path is suspect".to_owned()),
            scope,
        },
    )
    .await;
    sole_response(&sink).await
}

/// The operator picks the scope; origin stays `operator` so who paused and
/// what is held are reported separately — an all-spawns hold is not the
/// breaker.
#[tokio::test]
async fn all_spawns_pause_reports_scope_separately_from_origin() {
    let (server_state, _dir) = test_server_state();

    let response = pause_with_scope(&server_state, Some(DispatchPauseScope::AllSpawns)).await;

    match response {
        FrontendEvent::DispatchStateResult {
            paused,
            reviews_exempt,
            scope,
            origin,
            ..
        } => {
            assert!(paused);
            assert!(!reviews_exempt, "an all-spawns pause exempts nothing");
            assert_eq!(scope, Some(DispatchPauseScope::AllSpawns));
            assert_eq!(origin.as_deref(), Some("operator"));
        }
        other => panic!("expected DispatchStateResult, got {other:?}"),
    }
    let pause = server_state.execution_coordinator.dispatch_pause().expect("paused");
    assert_eq!(pause.origin, DispatchPauseOrigin::Operator);
    assert_eq!(pause.scope, DispatchPauseScope::AllSpawns);
    assert!(pause.reviews_held());
}

/// Omitting the scope is the long-standing default: reviews stay exempt.
#[tokio::test]
async fn default_pause_scope_keeps_reviews_exempt() {
    let (server_state, _dir) = test_server_state();

    let response = pause_with_scope(&server_state, None).await;

    match response {
        FrontendEvent::DispatchStateResult {
            reviews_exempt,
            scope,
            origin,
            ..
        } => {
            assert!(reviews_exempt, "the default operator pause must keep exempting reviews");
            assert_eq!(scope, Some(DispatchPauseScope::NewWork));
            assert_eq!(origin.as_deref(), Some("operator"));
        }
        other => panic!("expected DispatchStateResult, got {other:?}"),
    }
}

#[tokio::test]
async fn explicit_scope_changes_an_existing_pause_without_resuming() {
    let (state, _dir) = test_server_state();
    pause_with_scope(&state, None).await;
    let scope = DispatchPauseScope::AllSpawns;
    pause_with_scope(&state, Some(scope)).await;
    let pause = state.execution_coordinator.dispatch_pause().expect("still paused");
    assert_eq!(pause.scope, scope);
    assert_eq!(pause.origin, DispatchPauseOrigin::Operator);
    assert_eq!(
        state
            .work_db
            .get_metadata(METADATA_KEY_DISPATCH_PAUSE_SCOPE)
            .unwrap()
            .as_deref(),
        Some(scope.as_str())
    );
    // Taking ownership of a breaker hold must prevent automatic recovery.
    state.execution_coordinator.pause_dispatch(
        0,
        DispatchPauseOrigin::Breaker,
        boss_protocol::PauseReason::new("unhealthy spawn path").unwrap(),
    );
    pause_with_scope(&state, Some(DispatchPauseScope::AllSpawns)).await;
    let pause = state.execution_coordinator.dispatch_pause().expect("still paused");
    assert_eq!(pause.scope, DispatchPauseScope::AllSpawns);
    assert_eq!(pause.origin, DispatchPauseOrigin::Operator);
}

/// A running engine reports no scope or origin, and resuming clears the
/// persisted scope so a later pause cannot inherit it.
#[tokio::test]
async fn resume_reports_no_scope_and_clears_persisted_scope() {
    let (server_state, _dir) = test_server_state();
    pause_with_scope(&server_state, Some(DispatchPauseScope::AllSpawns)).await;

    let sink = make_session_sink();
    engine_meta::handle_set_dispatch_paused(
        dispatch(&server_state, &sink),
        FrontendRequest::SetDispatchPaused {
            paused: false,
            reason: None,
            scope: None,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::DispatchStateResult {
            paused, scope, origin, ..
        } => {
            assert!(!paused);
            assert_eq!(scope, None);
            assert_eq!(origin, None);
        }
        other => panic!("expected DispatchStateResult, got {other:?}"),
    }
    assert_eq!(
        server_state
            .work_db
            .get_metadata(METADATA_KEY_DISPATCH_PAUSE_SCOPE)
            .unwrap()
            .as_deref(),
        Some(""),
    );
}

/// The scope is persisted with the pause metadata and restored by the same
/// boot path the engine runs, so a restart cannot quietly turn an operator's
/// all-spawns hold back into the review-exempt default.
#[tokio::test]
async fn scope_survives_an_engine_restart_via_persisted_metadata() {
    for scope in [DispatchPauseScope::AllSpawns, DispatchPauseScope::NewWork] {
        let (server_state, dir) = test_server_state();
        pause_with_scope(&server_state, Some(scope)).await;
        assert_eq!(
            server_state
                .work_db
                .get_metadata(METADATA_KEY_DISPATCH_PAUSE_SCOPE)
                .unwrap()
                .as_deref(),
            Some(scope.as_str()),
        );

        // Reopen the database and construct a fresh server through the real
        // startup path; no in-memory pause state survives.
        drop(server_state);
        let cfg = Arc::new(RuntimeConfig::from_parts(
            boss_engine_runtime::config::WorkConfig::builder()
                .cwd(dir.path().to_path_buf())
                .db_path(dir.path().join("state.db"))
                .build(),
            None,
        ));
        let server_state =
            ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, ServerStateOverrides::default())
                .unwrap();

        let restored = server_state.execution_coordinator.dispatch_pause().expect("restored");
        assert_eq!(restored.scope, scope);
        assert_eq!(restored.origin, DispatchPauseOrigin::Operator);
        assert_eq!(restored.reviews_held(), scope == DispatchPauseScope::AllSpawns);
    }
}

/// An unrecognised persisted scope restores as the stricter one — never as
/// the review-exempt default, even for an operator-origin pause.
#[tokio::test]
async fn unrecognised_persisted_scope_restores_as_all_spawns() {
    for raw in ["a_scope_from_the_future", "", "   "] {
        let (server_state, _dir) = test_server_state();
        let db = &server_state.work_db;
        db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1").unwrap();
        db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_ORIGIN, "operator").unwrap();
        db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_SCOPE, raw).unwrap();

        assert!(restore_persisted_dispatch_pause(
            db,
            &server_state.execution_coordinator
        ));

        let restored = server_state.execution_coordinator.dispatch_pause().expect("restored");
        assert_eq!(restored.origin, DispatchPauseOrigin::Operator);
        assert_eq!(restored.scope, DispatchPauseScope::AllSpawns);
        assert!(restored.reviews_held());
    }
}

/// A pause persisted before the scope key existed behaved as its origin's
/// default, and must keep doing so across the upgrade.
#[tokio::test]
async fn pause_persisted_without_a_scope_restores_as_its_origins_default() {
    for (origin, expected) in [
        ("operator", DispatchPauseScope::NewWork),
        ("breaker", DispatchPauseScope::AllSpawns),
    ] {
        let (server_state, _dir) = test_server_state();
        let db = &server_state.work_db;
        db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1").unwrap();
        db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_ORIGIN, origin).unwrap();

        assert!(restore_persisted_dispatch_pause(
            db,
            &server_state.execution_coordinator
        ));
        assert_eq!(
            server_state
                .execution_coordinator
                .dispatch_pause()
                .expect("restored")
                .scope,
            expected,
            "origin {origin}",
        );
    }
}

/// The banner (engine-rendered title and body) names the scope for each of
/// the three kinds of pause.
#[tokio::test]
async fn health_banner_states_the_pause_scope() {
    let (server_state, _dir) = test_server_state();
    let coordinator = &server_state.execution_coordinator;
    let banner = |state: &Arc<ServerState>| {
        let report = build_engine_health_report(state);
        let issue = report
            .issues
            .into_iter()
            .find(|i| i.kind == "dispatch_paused")
            .expect("paused dispatch raises the banner");
        (issue.title, issue.body)
    };
    let reason = || boss_protocol::PauseReason::new("test").unwrap();

    coordinator.pause_dispatch_scoped(0, DispatchPauseOrigin::Operator, DispatchPauseScope::NewWork, reason());
    let (title, body) = banner(&server_state);
    assert!(title.contains("new work held, PR reviews exempt"), "{title}");
    assert!(body.contains("Scope: new work only"), "{body}");

    coordinator.pause_dispatch_scoped(
        0,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::AllSpawns,
        reason(),
    );
    let (title, body) = banner(&server_state);
    assert!(title.contains("all spawns held, PR reviews included"), "{title}");
    assert!(body.contains("Scope: all spawns (operator)"), "{body}");
    assert!(body.contains("except explicit `bossctl agents launch` overrides"));

    coordinator.pause_dispatch(0, DispatchPauseOrigin::Breaker, reason());
    let (title, body) = banner(&server_state);
    assert!(title.contains("all spawns held, PR reviews included"), "{title}");
    assert!(body.contains("Scope: all spawns (circuit breaker)"), "{body}");
}

#[tokio::test]
async fn weaker_pause_requests_preserve_existing_full_holds() {
    for origin in [DispatchPauseOrigin::Breaker, DispatchPauseOrigin::Operator] {
        for scope in [None, Some(DispatchPauseScope::NewWork)] {
            let (state, _dir) = test_server_state();
            state.execution_coordinator.pause_dispatch_scoped(
                42,
                origin,
                DispatchPauseScope::AllSpawns,
                boss_protocol::PauseReason::new("existing hold").unwrap(),
            );
            let before = state.execution_coordinator.dispatch_pause();
            pause_with_scope(&state, scope).await;
            assert_eq!(state.execution_coordinator.dispatch_pause(), before);
        }
    }
}

// ── review-pool reservations released by the handlers ─────────────────────

fn never_started_pre_merge_batch(state: &ServerState, project_id: &str, product_id: &str, name: &str) -> String {
    use boss_protocol::{ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile};
    let task = state
        .work_db
        .create_task(
            boss_protocol::CreateTaskInput::builder()
                .product_id(product_id)
                .project_id(project_id)
                .name(name)
                .autostart(false)
                .build(),
        )
        .unwrap();
    let input = boss_work_db::ReviewBatchCreateInput::builder()
        .cycle_root_id(task.id.clone())
        .base_sha("base-sha")
        .classification(
            ReviewClassification::builder()
                .changed_files(vec!["src/lib.rs".to_owned()])
                .complexity_flags(vec![])
                .has_production_code(true)
                .metadata_missing(vec![])
                .production_languages(vec![ReviewLanguageBucket::Rust])
                .profile(ReviewProfile::Light)
                .subsystem_buckets(vec!["src".to_owned()])
                .build(),
        )
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url(format!("https://github.com/example/repo/pull/{}", task.id))
        .target_sha("head-sha")
        .build();
    match state
        .work_db
        .create_pre_merge_review_batch(input, "https://github.com/example/repo")
        .unwrap()
    {
        boss_work_db::ReviewBatchDispatch::Created { batch, .. } => batch.id,
        other => panic!("expected a new batch, got {other:?}"),
    }
}

fn test_project(state: &ServerState, product_id: &str, name: &str) -> boss_protocol::Project {
    state
        .work_db
        .create_project(boss_protocol::CreateProjectInput {
            product_id: product_id.to_owned(),
            name: name.to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap()
}

/// Narrowing focus through `UpdateDispatchFocus` releases the never-started
/// out-of-scope reservation and keeps the in-scope one.
#[tokio::test]
async fn update_dispatch_focus_releases_never_started_out_of_scope_reservations() {
    let (state, _dir) = test_server_state();
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let keep = test_project(&state, &product.id, "Keep");
    let drop = test_project(&state, &product.id, "Drop");
    let kept_batch = never_started_pre_merge_batch(&state, &keep.id, &product.id, "In scope");
    let dropped_batch = never_started_pre_merge_batch(&state, &drop.id, &product.id, "Out of scope");

    let sink = make_session_sink();
    crate::app::dispatch_focus::handle_update_dispatch_focus(
        dispatch(&state, &sink),
        FrontendRequest::UpdateDispatchFocus {
            action: boss_protocol::DispatchFocusAction::Set,
            targets: vec![keep.id.clone()],
        },
    )
    .await;
    sole_response(&sink).await;

    assert!(state.work_db.review_batch(&dropped_batch).unwrap().is_none());
    assert!(state.work_db.review_batch(&kept_batch).unwrap().is_some());
}

/// An all-spawns `SetDispatchPaused` releases never-started reservations, but
/// a default (`new_work`) operator pause leaves them alone.
#[tokio::test]
async fn all_spawns_set_dispatch_paused_releases_never_started_reservations() {
    let (state, _dir) = test_server_state();
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let project = test_project(&state, &product.id, "Reviewed");
    let batch = never_started_pre_merge_batch(&state, &project.id, &product.id, "Pending review");

    pause_with_scope(&state, Some(DispatchPauseScope::NewWork)).await;
    assert!(state.work_db.review_batch(&batch).unwrap().is_some());

    pause_with_scope(&state, Some(DispatchPauseScope::AllSpawns)).await;
    assert!(state.work_db.review_batch(&batch).unwrap().is_none());
}

/// Cold start with a persisted pause: an operator all-spawns pause releases
/// never-started reservations; a scope-less operator pause (reviews exempt,
/// as the coordinator restores it) and a breaker pause leave them in place.
#[tokio::test]
async fn restoring_a_persisted_pause_releases_reservations_only_for_operator_all_spawns() {
    for (origin, scope, released) in [
        ("operator", Some("all_spawns"), true),
        ("operator", None, false),
        ("breaker", Some("all_spawns"), false),
        ("breaker", None, false),
    ] {
        let (state, _dir) = test_server_state();
        let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
        let project = test_project(&state, &product.id, "Reviewed");
        let batch = never_started_pre_merge_batch(&state, &project.id, &product.id, "Pending review");
        let db = &state.work_db;
        db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1").unwrap();
        db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_ORIGIN, origin).unwrap();
        if let Some(scope) = scope {
            db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_SCOPE, scope).unwrap();
        }

        assert!(restore_persisted_dispatch_pause(db, &state.execution_coordinator));

        assert_eq!(
            db.review_batch(&batch).unwrap().is_none(),
            released,
            "origin={origin} scope={scope:?}"
        );
    }
}
