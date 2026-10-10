//! Review-batch fan-out — within one batch and across batches of one cycle
//! root — must preserve the in-flight duplicate and writer guards.

use super::helpers::*;
use crate::work::{ReviewBatchCreateInput, ReviewBatchDispatch};
use boss_protocol::{ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile};

const REPO: &str = "git@github.com:spinyfin/mono.git";

fn batch(db: &WorkDb) -> Vec<WorkExecution> {
    let product = create_test_product(db);
    let root = create_test_chore_manual(db, product.id, "review target");
    batch_at(db, &root.id, "head-sha")
}

fn batch_at(db: &WorkDb, root_id: &str, target_sha: &str) -> Vec<WorkExecution> {
    let classification = ReviewClassification::builder()
        .changed_files(vec!["src/lib.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["src".to_owned()])
        .build();
    let input = ReviewBatchCreateInput::builder()
        .cycle_root_id(root_id)
        .base_sha("base-sha")
        .classification(classification)
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url("https://github.com/spinyfin/mono/pull/42")
        .target_sha(target_sha)
        .build();
    match db.create_pre_merge_review_batch(input, REPO).unwrap() {
        ReviewBatchDispatch::Created { executions, .. } => executions,
        other => panic!("expected a new batch: {other:?}"),
    }
}

#[tokio::test]
async fn both_review_batch_members_are_handed_off_in_one_drain_pass() {
    assert_single_pass_fanout(false).await;
}

#[tokio::test]
async fn existing_review_batch_reservation_does_not_filter_out_its_peers() {
    assert_single_pass_fanout(true).await;
}

async fn assert_single_pass_fanout(first_already_dispatching: bool) {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    for driver in ["claude", "codex"] {
        crate::test_support::insert_host_capability(&db, "local", &format!("driver={driver}"), "auto");
    }
    let executions = batch(&db);
    assert_eq!(executions.len(), 2);
    let cube = Arc::new(FakeCubeClient {
        slow_ensure_origin: Some(REPO.to_owned()),
        slow_ensure_delay: Duration::from_secs(600),
        ..FakeCubeClient::default()
    });
    let mut coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coordinator.set_review_pool(WorkerPool::new_review(3));
    let coordinator = Arc::new(coordinator);
    let _existing = first_already_dispatching.then(|| {
        assert_eq!(
            db.claim_execution_for_dispatch(&executions[0].id).unwrap(),
            DispatchClaimOutcome::Won
        );
        coordinator
            .inflight_dispatches
            .try_reserve(&executions[0], &db)
            .unwrap()
    });

    // No kick or heartbeat: the slow tails keep every reservation held for
    // the entire single pass, so sequential run completion cannot mask this.
    coordinator.drain_ready_queue().await;
    for execution in &executions {
        assert_eq!(
            db.get_execution(&execution.id).unwrap().status,
            ExecutionStatus::Claimed
        );
        assert!(coordinator.inflight_dispatches.blocks_execution(execution, &db));
    }
}

#[test]
fn duplicate_ready_review_execution_never_gets_a_second_reservation() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let executions = batch(&db);
    let inflight = InflightDispatches::new();
    let first = inflight.try_reserve(&executions[0], &db).unwrap();
    // The helper alone admits a leaf compared with itself. Reservation
    // identity must take precedence over that role-based exemption.
    assert!(
        db.are_admissible_concurrent_review_batch_pair(&executions[0].id, &executions[0].id)
            .unwrap()
    );
    assert!(inflight.try_reserve(&executions[0].clone(), &db).is_none());
    assert!(inflight.blocks_execution(&executions[0], &db));
    let second = inflight.try_reserve(&executions[1], &db).unwrap();
    drop(first);
    assert!(inflight.try_reserve(&executions[1], &db).is_none());
    drop(second);
    assert!(inflight.is_empty());
}

#[test]
fn ordinary_duplicate_and_other_root_batch_cannot_share_a_reservation() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let executions = batch(&db);
    let inflight = InflightDispatches::new();
    let _first = inflight.try_reserve(&executions[0], &db).unwrap();
    let mut duplicate = executions[0].clone();
    duplicate.id = "orphan-sweep-duplicate".to_owned();
    assert!(inflight.blocks_execution(&duplicate, &db));
    assert!(inflight.try_reserve(&duplicate, &db).is_none());
    // A batch member of a DIFFERENT cycle root whose execution row claims
    // this work item is not a reviewer of this root, whatever it says.
    let mut other_root_batch = batch(&db).remove(0);
    other_root_batch.work_item_id = executions[0].work_item_id.clone();
    assert!(inflight.blocks_execution(&other_root_batch, &db));
    assert!(inflight.try_reserve(&other_root_batch, &db).is_none());
}

/// A new head's batch on the SAME cycle root is not a duplicate of the
/// previous head's batch: its leaves must be handed off while the previous
/// batch's members are still in flight. Holding them would abandon the
/// latest head's reviewers as "redundant" against the previous batch's live
/// supervisor (mono PR #3110).
#[test]
fn a_new_heads_batch_on_the_same_cycle_root_shares_the_reservation() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let previous = batch(&db);
    let next = batch_at(&db, &previous[0].work_item_id, "next-head-sha");
    let inflight = InflightDispatches::new();
    let _previous = inflight.try_reserve(&previous[0], &db).unwrap();
    assert!(!inflight.blocks_execution(&next[0], &db));
    let _next = inflight.try_reserve(&next[0], &db).unwrap();
    assert!(!inflight.blocks_execution(&next[1], &db));
    assert!(inflight.try_reserve(&next[1], &db).is_some());
}

/// The spawn-time double-spawn guard (`schedule_execution`) is the second
/// place the redundancy decision is made; it must reach the same answer as
/// the drain pre-filter above. With the previous head's supervisor live on
/// the work item, a new head's leaf must proceed past the guard — never be
/// marked `abandoned` with a `redundant_spawn` timeline event.
#[tokio::test]
async fn spawn_guard_does_not_abandon_a_new_heads_leaf_behind_a_live_supervisor() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    for driver in ["claude", "codex"] {
        crate::test_support::insert_host_capability(&db, "local", &format!("driver={driver}"), "auto");
    }
    let previous = batch(&db);
    let root_id = previous[0].work_item_id.clone();
    // Reach `supervising` for the previous head through the real quorum
    // path, then make the supervisor the live execution on the work item.
    let previous_batch_id = db
        .review_batch_member_for_execution(&previous[0].id)
        .unwrap()
        .unwrap()
        .batch_id;
    for (index, execution) in previous.iter().enumerate() {
        db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &root_id,
            kind: boss_protocol::ProposalKind::ReviewReport,
            payload_json: &format!(
                r#"{{"batch_id":"{previous_batch_id}","target_sha":"head-sha","report":{{"batch_id":"{previous_batch_id}","pr_url":"https://github.com/spinyfin/mono/pull/42","target_sha":"head-sha","phase":"pre_merge","summary":"Clean.","coverage":{{"files_inspected":[],"files_omitted":[],"limitations":[]}},"findings":[]}}}}"#
            ),
            idempotency_key: &format!("report-{index}"),
        })
        .unwrap()
        .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();
    }
    let supervisor_id = db
        .review_batch_members(&previous_batch_id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == boss_protocol::ReviewBatchMemberRole::Supervisor)
        .and_then(|member| member.execution_id)
        .expect("quorum must create a supervisor execution");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'running', started_at = ?2 WHERE id = ?1",
            rusqlite::params![
                supervisor_id,
                boss_engine_utils::epoch_time::now_epoch_secs().to_string()
            ],
        )
        .unwrap();

    let next = batch_at(&db, &root_id, "next-head-sha");
    let leaf = next[0].clone();
    assert_eq!(
        db.claim_execution_for_dispatch(&leaf.id).unwrap(),
        DispatchClaimOutcome::Won
    );

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let mut coordinator = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner)
        .with_dispatch_events(recording.clone())
        .with_pre_start_retry_delays(Vec::new());
    coordinator.set_review_pool(WorkerPool::new_review(3));
    let coordinator = Arc::new(coordinator);
    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&leaf.id, None)
        .await
        .expect("worker pool slot available");

    let _ = coordinator
        .schedule_execution(&leaf, &worker_id, DispatchAdmission::Queued)
        .await;

    let events = recording.events_for(&leaf.id).await;
    assert!(
        !events
            .iter()
            .any(|e| e.details.get("reason").and_then(|v| v.as_str()) == Some("redundant_spawn")),
        "a new head's leaf must not be judged redundant against the previous head's supervisor; got {events:#?}",
    );
    assert_ne!(
        db.get_execution(&leaf.id).unwrap().status,
        ExecutionStatus::Abandoned,
        "the new head's leaf must not be abandoned at dispatch",
    );
    assert_eq!(
        db.get_execution(&supervisor_id).unwrap().status,
        ExecutionStatus::Running,
        "the previous head's supervisor must be left alone",
    );
}

#[test]
fn racing_ready_review_rows_have_exactly_one_reservation_owner() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let executions = batch(&db);
    let inflight = InflightDispatches::new();
    let start = std::sync::Barrier::new(8);
    let held = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        let attempts: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    let reservation = inflight.try_reserve(&executions[0], &db);
                    // Keep the winner alive until every competing caller has tried.
                    held.wait();
                    reservation.is_some()
                })
            })
            .collect();
        assert_eq!(
            attempts
                .into_iter()
                .map(|attempt| attempt.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
    });
    assert!(inflight.is_empty());
}

#[test]
fn unavailable_batch_membership_fails_closed() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let executions = batch(&db);
    let inflight = InflightDispatches::new();
    let _first = inflight.try_reserve(&executions[0], &db).unwrap();
    db.connect()
        .unwrap()
        .execute("ALTER TABLE pr_review_batch_members RENAME TO unavailable_members", [])
        .unwrap();
    assert!(inflight.blocks_execution(&executions[1], &db));
    assert!(inflight.try_reserve(&executions[1], &db).is_none());
    assert!(inflight.blocks_execution(&executions[0], &db));
}
