//! Pre-start spawn-failure streak alerts, driven through the real dispatch
//! path: `run_execution`'s failure arm must feed the per-(driver, worker
//! kind) tracker, its success arm must resolve the alert, and combinations
//! must not merge. The tracker's own arithmetic is unit-tested in
//! [`crate::pre_start_streak`]; these tests pin the wiring.
//!
//! The scenario is incident 008's: Codex review-guide spawns refused before
//! a pane exists, repeatedly, with nothing succeeding in between.
//!
//! Shared fixtures live in [`super::helpers`].

use std::sync::atomic::AtomicUsize;

use super::helpers::*;
use crate::coordinator::PANE_SPAWN_FAILED_ATTENTION_KIND;

/// The refusal text every incident-008 spawn failed with.
const HOOK_TRUST_REFUSAL: &str =
    "codex hook-trust gate refused the spawn: hooks/list returned no hook entries; silence is not success";

/// A coordinator with a one-slot review pool whose runner fails its first
/// `failures` spawns with [`HOOK_TRUST_REFUSAL`] and then succeeds.
fn coordinator_failing_first(db: &Arc<WorkDb>, failures: usize) -> Arc<ExecutionCoordinator> {
    let runner = Arc::new(FakeExecutionRunner {
        fail_remaining: AtomicUsize::new(failures),
        fail_message: Some(HOOK_TRUST_REFUSAL.to_owned()),
        ..FakeExecutionRunner::default()
    });
    let mut coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        runner,
    );
    coordinator.set_review_pool(WorkerPool::new_review(1));
    Arc::new(coordinator)
}

fn review_guide_db() -> (tempfile::TempDir, Arc<WorkDb>, String) {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    crate::test_support::insert_host_capability(&db, "local", "driver=codex", "auto");
    let product = create_product(&db);
    (dir, db, product)
}

/// Mint a ready `pr_review_guide` execution for PR `pr_number` on its own
/// root chore, the way source capture does in production (one comparison
/// per PR, one PR per root).
fn create_guide_execution(db: &WorkDb, product: &str, pr_number: u64) -> WorkExecution {
    // Manual chore (autostart off) so reconciliation never mints an ordinary
    // execution for the root itself.
    let root = create_test_chore_manual(db, product.to_owned(), format!("PR {pr_number}")).id;
    let mut packet = crate::test_support::review_guide_source_packet("base", "head");
    packet.canonical_pr_url = format!("https://github.com/acme/widget/pull/{pr_number}");
    packet.pr_number = pr_number;
    let stored = db
        .persist_pr_review_guide_source_capture(&root, 1, crate::work::PrSourceCaptureTrigger::Creation, &packet)
        .unwrap();
    let crate::work::PrSourceCapturePersistOutcome::Stored(capture) = stored else {
        panic!("capture must persist")
    };
    db.create_pr_review_guide_execution(&capture.comparison_id, "https://github.com/test/repo")
        .unwrap()
}

/// Dispatch one more review-guide spawn and wait for it to reach `expected`.
async fn run_guide(
    coordinator: &Arc<ExecutionCoordinator>,
    db: &WorkDb,
    product: &str,
    pr_number: u64,
    expected: ExecutionStatus,
) -> WorkExecution {
    let execution = create_guide_execution(db, product, pr_number);
    coordinator.kick();
    wait_for_execution_status(db, &execution.id, expected).await;
    execution
}

/// Incident 008 end to end: consecutive Codex review-guide pre-start
/// failures raise exactly one alert carrying the latest error, further
/// failures update that same alert, and the next successful review-guide
/// spawn resolves it.
#[tokio::test]
async fn consecutive_codex_review_guide_failures_raise_one_alert_until_a_success() {
    let (_dir, db, product) = review_guide_db();
    let coordinator = coordinator_failing_first(&db, 3);
    let streaks = coordinator.pre_start_streaks().clone();

    // One failure is not a streak.
    let first = run_guide(&coordinator, &db, &product, 1, ExecutionStatus::Failed).await;
    assert!(
        streaks.active_alerts().is_empty(),
        "a single pre-start failure must not raise an alert: {:?}",
        streaks.active_alerts()
    );

    // The second consecutive failure raises it.
    let second = run_guide(&coordinator, &db, &product, 2, ExecutionStatus::Failed).await;
    let alerts = streaks.active_alerts();
    assert_eq!(alerts.len(), 1, "exactly one alert for the combination: {alerts:?}");
    assert_eq!(alerts[0].driver, "codex");
    assert_eq!(alerts[0].worker_kind, "review-guide");
    assert_eq!(alerts[0].consecutive_failures, 2);
    assert_eq!(alerts[0].latest_execution_id, second.id);
    assert!(
        alerts[0].latest_error.contains(HOOK_TRUST_REFUSAL),
        "the alert must carry the refusal text: {}",
        alerts[0].latest_error
    );
    let first_failure_at = alerts[0].first_failure_epoch_s;

    // A third failure updates the same alert in place: live count, newest
    // execution, first-failure time unchanged. Never a second alert.
    let third = run_guide(&coordinator, &db, &product, 3, ExecutionStatus::Failed).await;
    let alerts = streaks.active_alerts();
    assert_eq!(
        alerts.len(),
        1,
        "further failures must not open a second alert: {alerts:?}"
    );
    assert_eq!(alerts[0].consecutive_failures, 3);
    assert_eq!(alerts[0].latest_execution_id, third.id);
    assert_eq!(alerts[0].first_failure_epoch_s, first_failure_at);

    // The alert is additive: each failed guide spawn still gets its own
    // execution-scoped `pane_spawn_failed` attention item, and stays
    // terminal `failed` — the streak neither replaces nor softens that.
    for execution in [&first, &second, &third] {
        let attention = db.list_attention_items(&execution.id).unwrap();
        assert!(
            attention
                .iter()
                .any(|item| item.kind == PANE_SPAWN_FAILED_ATTENTION_KIND && item.status == "open"),
            "review-guide execution {} must keep its per-execution attention item: {attention:?}",
            execution.id
        );
    }
    assert!(
        !coordinator.is_dispatch_paused(),
        "the streak alert is visibility only and must never pause dispatch"
    );

    // The next review-guide spawn succeeds, which resolves the alert.
    run_guide(&coordinator, &db, &product, 4, ExecutionStatus::Running).await;
    assert!(
        streaks.active_alerts().is_empty(),
        "a successful codex review-guide spawn must resolve the alert: {:?}",
        streaks.active_alerts()
    );
}

/// The codex review-guide alert, if one is active.
fn review_guide_alert(coordinator: &ExecutionCoordinator) -> Option<boss_protocol::SpawnFailureStreak> {
    coordinator
        .pre_start_streaks()
        .active_alerts()
        .into_iter()
        .find(|alert| alert.driver == "codex" && alert.worker_kind == "review-guide")
}

/// Failures for a different (driver, worker kind) combination count
/// separately: a failing Claude implementation worker between two Codex
/// review-guide failures neither pushes the review-guide combination over
/// the threshold early nor inflates its count. (That another combination's
/// *success* does not resolve the alert is pinned in
/// `crate::pre_start_streak`'s unit tests.)
#[tokio::test]
async fn failures_for_a_different_combination_do_not_merge_into_the_alert() {
    let (_dir, db, product) = review_guide_db();
    // Every spawn fails, whatever its kind.
    let coordinator = coordinator_failing_first(&db, usize::MAX);

    // One Codex review-guide failure, then one failure of an ordinary
    // Claude implementation worker: two pre-start failures fleet-wide, back
    // to back — but only one of them is a Codex review-guide failure.
    run_guide(&coordinator, &db, &product, 1, ExecutionStatus::Failed).await;
    let chore = create_test_chore(&db, product.clone(), "Ordinary chore");
    db.reconcile_product_executions(&product).unwrap();
    let chore_execution = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &chore_execution, ExecutionStatus::Failed).await;
    assert!(
        review_guide_alert(&coordinator).is_none(),
        "another combination's failure must not count towards the review-guide streak: {:?}",
        coordinator.pre_start_streaks().active_alerts()
    );

    // The second review-guide failure raises the alert, at exactly 2: the
    // intervening Claude failure neither reset nor added to the count.
    let second = run_guide(&coordinator, &db, &product, 2, ExecutionStatus::Failed).await;
    let alert = review_guide_alert(&coordinator).expect("two consecutive review-guide failures raise the alert");
    assert_eq!(alert.consecutive_failures, 2, "{alert:?}");
    assert_eq!(alert.latest_execution_id, second.id);
    assert_ne!(
        alert.latest_execution_id, chore_execution,
        "the alert must only ever cite a review-guide execution"
    );
}

/// A `SlotBusy` rejection is an engine/app slot desync, not evidence that a
/// driver or worker kind cannot start, so it never counts towards a streak:
/// one genuine failure followed by a slot-busy rejection for the same
/// combination is still one failure, not two.
#[tokio::test]
async fn slot_busy_rejections_never_count_towards_a_streak() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);

    // One genuine failure, then every later spawn is rejected `SlotBusy`.
    let runner = Arc::new(FakeExecutionRunner {
        fail_remaining: AtomicUsize::new(1),
        slot_busy: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        runner.clone(),
    ));

    let first = create_test_chore(&db, product.id.clone(), "Genuinely fails");
    db.reconcile_product_executions(&product.id).unwrap();
    let first_execution = db.list_executions(Some(&first.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &first_execution, ExecutionStatus::Failed).await;

    let second = create_test_chore(&db, product.id.clone(), "Rejected slot-busy");
    db.reconcile_product_executions(&product.id).unwrap();
    let second_execution = db.list_executions(Some(&second.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &second_execution, ExecutionStatus::Failed).await;

    assert!(
        runner.calls.lock().await.len() >= 2,
        "fixture precondition: both spawns must have reached the runner"
    );
    assert!(
        coordinator.pre_start_streaks().active_alerts().is_empty(),
        "a slot-busy rejection must not count as the second consecutive failure: {:?}",
        coordinator.pre_start_streaks().active_alerts()
    );
}
