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
use crate::coordinator::DispatchPauseOrigin;
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
        let (server_state, _dir) = test_server_state();
        pause_with_scope(&server_state, Some(scope)).await;
        assert_eq!(
            server_state
                .work_db
                .get_metadata(METADATA_KEY_DISPATCH_PAUSE_SCOPE)
                .unwrap()
                .as_deref(),
            Some(scope.as_str()),
        );

        // "Restart": the in-memory pause is gone, the metadata is not.
        server_state.execution_coordinator.resume_dispatch();
        assert!(!server_state.execution_coordinator.is_dispatch_paused());
        assert!(restore_persisted_dispatch_pause(
            &server_state.work_db,
            &server_state.execution_coordinator
        ));

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
    let (server_state, _dir) = test_server_state();
    let db = &server_state.work_db;
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1").unwrap();
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_ORIGIN, "operator").unwrap();
    db.set_metadata(METADATA_KEY_DISPATCH_PAUSE_SCOPE, "a_scope_from_the_future")
        .unwrap();

    assert!(restore_persisted_dispatch_pause(
        db,
        &server_state.execution_coordinator
    ));

    let restored = server_state.execution_coordinator.dispatch_pause().expect("restored");
    assert_eq!(restored.origin, DispatchPauseOrigin::Operator);
    assert_eq!(restored.scope, DispatchPauseScope::AllSpawns);
    assert!(restored.reviews_held());
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

    coordinator.pause_dispatch(0, DispatchPauseOrigin::Breaker, reason());
    let (title, body) = banner(&server_state);
    assert!(title.contains("all spawns held, PR reviews included"), "{title}");
    assert!(body.contains("Scope: all spawns (circuit breaker)"), "{body}");
}
