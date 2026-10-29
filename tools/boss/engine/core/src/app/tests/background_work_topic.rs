// The toolbar badge is fed by `engine.background_work` pushes, not by the app
// polling `list_engine_attempts`. These tests drive the real inputs (planner
// run writes, conflict-rung writes plus the lease registry) and assert on what
// a subscribed session receives.

use super::*;

use boss_protocol::{BackgroundWorkItem, BackgroundWorkKind, CreateProjectInput, TOPIC_ENGINE_BACKGROUND_WORK};

use crate::test_support::{create_test_chore_manual, create_test_product_with_repo};
use crate::work::{ClaimPlannerRunInput, ConflictResolutionInsertInput};

fn dispatch_for(state: &Arc<ServerState>, sink: &Arc<SessionSink>, session_id: &str) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id(session_id)
        .request_id("req-subscribe")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

/// Register a session and subscribe it to the topic through the real
/// handler, exactly as the app does on connect.
async fn subscribe_session(state: &Arc<ServerState>, session_id: &str) -> Arc<SessionSink> {
    let sink = make_session_sink();
    state.topic_broker.register_session(session_id, sink.clone()).await;
    crate::app::subscriptions::handle_subscribe(
        dispatch_for(state, &sink, session_id),
        FrontendRequest::Subscribe {
            topics: vec![TOPIC_ENGINE_BACKGROUND_WORK.to_owned()],
        },
    )
    .await;
    sink
}

async fn next_envelope(sink: &SessionSink) -> FrontendEventEnvelope {
    tokio::time::timeout(std::time::Duration::from_secs(10), sink.next())
        .await
        .expect("no push arrived within 10s — the running app's badge would not update")
        .expect("sink closed before the push arrived")
}

async fn next_snapshot(sink: &SessionSink) -> Vec<BackgroundWorkItem> {
    match next_envelope(sink).await.payload {
        FrontendEvent::BackgroundWorkSnapshot { items } => items,
        other => panic!("expected BackgroundWorkSnapshot, got {other:?}"),
    }
}

/// Subscribe and consume the replayed snapshot and the `Subscribed` reply.
async fn subscribe_and_drain(
    state: &Arc<ServerState>,
    session_id: &str,
) -> (Arc<SessionSink>, Vec<BackgroundWorkItem>) {
    let sink = subscribe_session(state, session_id).await;
    let replayed = next_snapshot(&sink).await;
    assert!(matches!(
        next_envelope(&sink).await.payload,
        FrontendEvent::Subscribed { .. }
    ));
    (sink, replayed)
}

fn seed_project(db: &WorkDb, name: &str) -> (String, String) {
    let product = create_test_product_with_repo(db, "Test", Some("git@github.com:test/test.git"));
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name(name)
                .goal("build it")
                .build(),
        )
        .unwrap();
    (product.id, project.id)
}

fn claim(db: &WorkDb, product_id: &str, project_id: &str) -> String {
    db.claim_planner_run(ClaimPlannerRunInput {
        project_id,
        product_id,
        design_task_id: None,
        caller: "test",
    })
    .unwrap()
    .expect("claim")
    .id
}

/// Age a run past the anti-flicker gate. A raw write: it announces nothing, as
/// the passage of time announces nothing.
fn backdate(db: &WorkDb, run_id: &str, secs_ago: i64) {
    let now: i64 = crate::work::now_string().parse().unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE planner_runs SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![run_id, (now - secs_ago).to_string()],
        )
        .unwrap();
}

fn finish(db: &WorkDb, run_id: &str) {
    db.update_planner_run(
        run_id,
        crate::work::PlannerRunPatch {
            outcome: Some(boss_protocol::PLANNER_OUTCOME_STAGED.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
}

fn touch(db: &WorkDb, run_id: &str) {
    db.update_planner_run(
        run_id,
        crate::work::PlannerRunPatch {
            notes: Some("progress".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
}

/// A mechanical rung as `conflict_ladder` runs it: stamp the marker, register
/// the lease, announce it.
fn begin_rung(db: &WorkDb, attempt_id: &str, lease_id: &str, workspace_id: &str) {
    db.stamp_conflict_resolution_mechanical_rung(attempt_id, 1, lease_id, workspace_id)
        .unwrap();
    crate::ladder_lease_registry::register(&crate::coordinator::CubeWorkspaceLease {
        lease_id: lease_id.to_owned(),
        workspace_id: workspace_id.to_owned(),
        workspace_path: std::path::PathBuf::from("/tmp/background-work-topic-test"),
        dirty_verified: None,
    });
    db.notify_background_work_changed();
}

#[tokio::test]
async fn a_subscribing_session_is_replayed_the_current_snapshot() {
    let (state, _dir) = test_server_state();
    let (product_id, project_id) = seed_project(&state.work_db, "Alpha");
    let run_id = claim(&state.work_db, &product_id, &project_id);
    backdate(&state.work_db, &run_id, 20);

    let (_sink, replayed) = subscribe_and_drain(&state, "session-a").await;

    assert_eq!(replayed.len(), 1, "got {replayed:?}");
    assert_eq!(replayed[0].kind, BackgroundWorkKind::ProjectPlanner);
    assert_eq!(replayed[0].source_id, run_id);
    assert_eq!(replayed[0].phase, "Planning Alpha");
}

#[tokio::test]
async fn resubscribing_replays_again() {
    let (state, _dir) = test_server_state();
    let (_sink, first) = subscribe_and_drain(&state, "session-a").await;
    assert!(first.is_empty());

    // A resync re-sends the subscription; the topic is already held.
    crate::app::subscriptions::handle_subscribe(
        dispatch_for(&state, &_sink, "session-a"),
        FrontendRequest::Subscribe {
            topics: vec![TOPIC_ENGINE_BACKGROUND_WORK.to_owned()],
        },
    )
    .await;
    assert!(next_snapshot(&_sink).await.is_empty());
}

#[tokio::test]
async fn planner_run_transitions_each_publish_one_snapshot() {
    let (state, _dir) = test_server_state();
    let _publisher = state.spawn_background_work_publisher();
    let (sink, initial) = subscribe_and_drain(&state, "session-a").await;
    assert!(initial.is_empty());
    let db = &state.work_db;
    let (product_id, project_id) = seed_project(db, "Alpha");

    // Claimed: younger than the anti-flicker gate, so the set is unchanged and
    // nothing is pushed. The next envelope must be the one below, not a
    // redundant empty snapshot.
    let run_id = claim(db, &product_id, &project_id);
    backdate(db, &run_id, 20);
    // Updated: now old enough to list.
    touch(db, &run_id);
    let listed = next_snapshot(&sink).await;
    assert_eq!(listed.len(), 1, "got {listed:?}");
    assert_eq!(listed[0].source_id, run_id);

    // Finished.
    finish(db, &run_id);
    assert!(next_snapshot(&sink).await.is_empty());

    // Deleted while running.
    db.delete_planner_run(&run_id).unwrap();
    let second = claim(db, &product_id, &project_id);
    backdate(db, &second, 20);
    touch(db, &second);
    assert_eq!(next_snapshot(&sink).await.len(), 1);
    db.delete_planner_run(&second).unwrap();
    assert!(next_snapshot(&sink).await.is_empty());
}

#[tokio::test]
async fn mechanical_rung_transitions_each_publish_one_snapshot() {
    let (state, _dir) = test_server_state();
    let _publisher = state.spawn_background_work_publisher();
    let (sink, initial) = subscribe_and_drain(&state, "session-a").await;
    assert!(initial.is_empty());
    let db = &state.work_db;
    let (product_id, _project_id) = seed_project(db, "Alpha");

    // Concluded: the ladder clears the marker, then unregisters the lease.
    let attempt = seed_attempt_for_pr(db, &product_id, 1);
    begin_rung(db, &attempt, "bgtopic-lease-clear", "bgtopic-ws-clear");
    let listed = next_snapshot(&sink).await;
    assert_eq!(listed.len(), 1, "got {listed:?}");
    assert_eq!(listed[0].kind, BackgroundWorkKind::ConflictRemediation);
    assert_eq!(listed[0].source_id, attempt);
    assert_eq!(listed[0].phase, "Rebasing Chore 1");
    db.clear_conflict_resolution_mechanical_rung(&attempt).unwrap();
    assert!(next_snapshot(&sink).await.is_empty());
    crate::ladder_lease_registry::unregister("bgtopic-lease-clear");

    // Re-stamped to rung 0: the phase text changes.
    let attempt = seed_attempt_for_pr(db, &product_id, 2);
    begin_rung(db, &attempt, "bgtopic-lease-rung0", "bgtopic-ws-rung0");
    assert_eq!(next_snapshot(&sink).await[0].phase, "Rebasing Chore 2");
    db.stamp_conflict_resolution_mechanical_rung(&attempt, 0, "bgtopic-lease-rung0", "bgtopic-ws-rung0")
        .unwrap();
    assert_eq!(next_snapshot(&sink).await[0].phase, "Applying deterministic resolution");

    // Retired mid-rung.
    db.mark_conflict_resolution_succeeded_at_rung(&attempt, None, 0)
        .unwrap();
    assert!(next_snapshot(&sink).await.is_empty());
    crate::ladder_lease_registry::unregister("bgtopic-lease-rung0");
}

#[tokio::test]
async fn abandoned_and_failed_attempts_leave_the_snapshot() {
    let (state, _dir) = test_server_state();
    let _publisher = state.spawn_background_work_publisher();
    let (sink, _) = subscribe_and_drain(&state, "session-a").await;
    let db = &state.work_db;
    let (product_id, _project_id) = seed_project(db, "Alpha");

    for (n, abandon) in [(3, true), (4, false)] {
        let lease = format!("bgtopic-lease-end-{n}");
        let attempt = seed_attempt_for_pr(db, &product_id, n);
        begin_rung(db, &attempt, &lease, &format!("bgtopic-ws-end-{n}"));
        assert_eq!(next_snapshot(&sink).await.len(), 1);
        if abandon {
            db.mark_conflict_resolution_abandoned(&attempt, "parent merged")
                .unwrap();
        } else {
            db.mark_conflict_resolution_failed(&attempt, "rebase failed").unwrap();
        }
        assert!(next_snapshot(&sink).await.is_empty());
        crate::ladder_lease_registry::unregister(&lease);
    }
}

#[tokio::test]
async fn every_subscribed_session_gets_the_same_push() {
    let (state, _dir) = test_server_state();
    let _publisher = state.spawn_background_work_publisher();
    let (first, _) = subscribe_and_drain(&state, "session-a").await;
    let (second, _) = subscribe_and_drain(&state, "session-b").await;
    let (product_id, project_id) = seed_project(&state.work_db, "Alpha");
    let run_id = claim(&state.work_db, &product_id, &project_id);
    backdate(&state.work_db, &run_id, 20);
    touch(&state.work_db, &run_id);

    let a = next_snapshot(&first).await;
    let b = next_snapshot(&second).await;
    assert_eq!(a, b);
    assert_eq!(a.len(), 1);
}

#[tokio::test]
async fn a_run_still_inside_the_age_gate_schedules_its_own_wake() {
    let (state, _dir) = test_server_state();
    let db = &state.work_db;
    let (product_id, project_id) = seed_project(db, "Alpha");
    assert_eq!(state.topic_broker.refresh_background_work(db).await, None);

    let run_id = claim(db, &product_id, &project_id);
    backdate(db, &run_id, 10);
    let wake = state
        .topic_broker
        .refresh_background_work(db)
        .await
        .expect("a hidden running planner run needs a wake");
    assert!(
        (4..=5).contains(&wake.as_secs()),
        "a run 10s old is listed 5s from now, got {wake:?}"
    );

    // Past the gate nothing is waiting any more.
    backdate(db, &run_id, 20);
    assert_eq!(state.topic_broker.refresh_background_work(db).await, None);
}

fn seed_attempt_for_pr(db: &WorkDb, product_id: &str, pr_number: i64) -> String {
    let work_item_id = create_test_chore_manual(db, product_id.to_owned(), format!("Chore {pr_number}")).id;
    db.insert_conflict_resolution(
        ConflictResolutionInsertInput::builder()
            .product_id(product_id)
            .work_item_id(work_item_id)
            .pr_url(format!("https://github.com/test/test/pull/{pr_number}"))
            .pr_number(pr_number)
            .head_branch(format!("feature-{pr_number}"))
            .base_branch("main")
            .base_sha_at_trigger("abc")
            .head_sha_before("def")
            .build(),
    )
    .unwrap()
    .unwrap()
    .id
}
