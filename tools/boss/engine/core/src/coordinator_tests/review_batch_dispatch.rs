//! Same-batch fan-out must preserve the in-flight duplicate and writer guards.

use super::helpers::*;
use crate::work::{ReviewBatchCreateInput, ReviewBatchDispatch};
use boss_protocol::{ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile};

const REPO: &str = "git@github.com:spinyfin/mono.git";

fn batch(db: &WorkDb) -> Vec<WorkExecution> {
    let product = create_test_product(db);
    let root = create_test_chore_manual(db, product.id, "review target");
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
        .cycle_root_id(root.id)
        .base_sha("base-sha")
        .classification(classification)
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url("https://github.com/spinyfin/mono/pull/42")
        .target_sha("head-sha")
        .build();
    match db.create_pre_merge_review_batch(input, REPO).unwrap() {
        ReviewBatchDispatch::Created { executions, .. } => executions,
        other => panic!("expected a new batch: {other:?}"),
    }
}

#[tokio::test]
async fn all_three_review_batch_members_are_handed_off_in_one_drain_pass() {
    assert_single_pass_fanout(false).await;
}

#[tokio::test]
async fn existing_review_batch_reservation_does_not_filter_out_its_peers() {
    assert_single_pass_fanout(true).await;
}

async fn assert_single_pass_fanout(first_already_dispatching: bool) {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    for driver in ["claude", "codex", "grok"] {
        crate::test_support::insert_host_capability(&db, "local", &format!("driver={driver}"), "auto");
    }
    let executions = batch(&db);
    assert_eq!(executions.len(), 3);
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
        db.are_admissible_same_review_batch_pair(&executions[0].id, &executions[0].id)
            .unwrap()
    );
    assert!(inflight.try_reserve(&executions[0].clone(), &db).is_none());
    assert!(inflight.blocks_execution(&executions[0], &db));
    let second = inflight.try_reserve(&executions[1], &db).unwrap();
    let third = inflight.try_reserve(&executions[2], &db).unwrap();
    drop(first);
    assert!(inflight.try_reserve(&executions[1], &db).is_none());
    assert!(inflight.try_reserve(&executions[2], &db).is_none());
    drop((second, third));
    assert!(inflight.is_empty());
}

#[test]
fn ordinary_duplicate_and_other_batch_cannot_share_a_reservation() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let executions = batch(&db);
    let inflight = InflightDispatches::new();
    let _first = inflight.try_reserve(&executions[0], &db).unwrap();
    let mut duplicate = executions[0].clone();
    duplicate.id = "orphan-sweep-duplicate".to_owned();
    assert!(inflight.blocks_execution(&duplicate, &db));
    assert!(inflight.try_reserve(&duplicate, &db).is_none());
    let mut other_batch = batch(&db).remove(0);
    other_batch.work_item_id = executions[0].work_item_id.clone();
    assert!(inflight.blocks_execution(&other_batch, &db));
    assert!(inflight.try_reserve(&other_batch, &db).is_none());
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
