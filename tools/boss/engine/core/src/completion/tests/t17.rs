//! A stamped `run_done` declaration must reach a terminal write without the
//! worker doing anything further: transient DB failures, an inconclusive
//! GitHub answer after one, a lost retry task (recovered by the sweep) and a
//! failed `no-changes-needed` write are all retried to a terminal state.

use super::*;

const OWN_PR: &str = "https://github.com/spinyfin/mono/pull/1764";

fn own_branch(execution_id: &str) -> String {
    expected_branch_name(execution_id, &BranchNaming::BossExecPrefix, None)
}

fn flags_on(workspace: &Path) -> Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.join("feature-flags.toml"),
    ));
    flags.set("worker_proposals", true).unwrap();
    flags.set("run_done_proposals_seam", true).unwrap();
    flags
}

fn task_row(db: &WorkDb, id: &str) -> boss_protocol::Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("expected a task row, got {other:?}"),
    }
}

fn stamp_delivered(db: &WorkDb, execution_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'delivered', run_done_declared_at = '1' WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();
}

const FAIL_TERMINAL_WRITE: &str = "CREATE TRIGGER fail_finalize BEFORE UPDATE OF status ON work_executions
     WHEN NEW.status IN ('completed', 'failed')
     BEGIN SELECT RAISE(ABORT, 'database is locked'); END;";

/// Detector that parks every lookup until released, holding one finalize
/// in flight.
struct GatedPrDetector {
    inner: Arc<StubPrDetector>,
    entered: std::sync::atomic::AtomicUsize,
    release: tokio::sync::Notify,
}

#[async_trait]
impl PrDetector for GatedPrDetector {
    async fn detect_pr(&self, repo_remote_url: &str, expected_branch: &str) -> Result<PrStatus> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.release.notified().await;
        self.inner.detect_pr(repo_remote_url, expected_branch).await
    }
}

/// A non-transient failure of the terminalizing write keeps the run live and
/// retrying; once it clears the run ends finalized with its slot released.
#[tokio::test]
async fn delivered_finalize_retries_a_failing_write_until_it_commits() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    stamp_delivered(&db, &execution_id);
    db.connect().unwrap().execute_batch(FAIL_TERMINAL_WRITE).unwrap();
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)));
    let handler = Arc::new(handler.with_branch_verifier(StubBranchVerifier::ok(&own_branch(&execution_id))));

    let first = handler
        .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
        .await;
    assert_eq!(first, StopOutcome::DbError);
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());

    let retrying = tokio::spawn({
        let handler = Arc::clone(&handler);
        let execution_id = execution_id.clone();
        async move {
            handler
                .finalize_declared_run_done_until_resolved(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
                .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!retrying.is_finished(), "still failing, so still retrying");
    db.connect()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_finalize;")
        .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), retrying)
        .await
        .expect("retry commits once the failure clears")
        .unwrap();
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "{outcome:?}"
    );
    let task = task_row(&db, &chore_id);
    assert_eq!(task.pr_url.as_deref(), Some(OWN_PR));
    assert_ne!(task.status, TaskStatus::Todo, "{task:?}");
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
}

/// A genuine SQLITE_BUSY (another connection holds the write lock past the
/// busy timeout) is retried and the run ends finalized once it is released.
#[tokio::test]
async fn delivered_finalize_survives_a_genuine_sqlite_busy() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    stamp_delivered(&db, &execution_id);
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)));
    let handler = Arc::new(handler.with_branch_verifier(StubBranchVerifier::ok(&own_branch(&execution_id))));

    let path = db.path().to_path_buf();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked_tx.send(()).unwrap();
        // Longer than the engine's 5s busy timeout, so the first attempt fails.
        std::thread::sleep(std::time::Duration::from_millis(5500));
        conn.execute_batch("ROLLBACK").unwrap();
    });
    locked_rx.recv().unwrap();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        handler.finalize_declared_run_done_until_resolved(&execution_id, boss_protocol::RunDoneOutcome::Delivered),
    )
    .await
    .expect("finalize resolves once the lock is released");
    holder.join().unwrap();
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == OWN_PR),
        "{outcome:?}"
    );
    assert_eq!(task_row(&db, &chore_id).pr_url.as_deref(), Some(OWN_PR));
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
}

/// After a DB failure, a transient GitHub failure on the next attempt must
/// not end the retry: ownership is kept until a terminal commit.
#[tokio::test]
async fn retry_outlives_a_transient_pr_resolution_failure_after_a_db_failure() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    stamp_delivered(&db, &execution_id);
    db.connect().unwrap().execute_batch(FAIL_TERMINAL_WRITE).unwrap();
    let detector = StubPrDetector::ok(Some(OWN_PR));
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = Arc::new(handler.with_branch_verifier(StubBranchVerifier::ok(&own_branch(&execution_id))));

    let retrying = tokio::spawn({
        let handler = Arc::clone(&handler);
        let execution_id = execution_id.clone();
        async move {
            handler
                .finalize_declared_run_done_until_resolved(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
                .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // The DB recovers, but GitHub now cannot answer.
    db.connect()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_finalize;")
        .unwrap();
    detector.set_error("gh: connect: network is unreachable").await;
    tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
    assert!(!retrying.is_finished(), "a DetectorFailed attempt is not a resolution");
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());

    detector.set_result(PrStatus::Fresh { url: OWN_PR.to_owned() }).await;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), retrying)
        .await
        .expect("retry commits once GitHub answers")
        .unwrap();
    assert!(matches!(outcome, StopOutcome::PrDetected { .. }), "{outcome:?}");
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
}

/// A stamped declaration whose retry task was lost (engine restart, panic) is
/// recovered by the durable sweep run against a freshly built handler.
#[tokio::test]
async fn sweep_finalizes_a_stamped_declaration_after_handler_recreation() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    stamp_delivered(&db, &execution_id);
    // A declaration made moments ago is left to the submission path.
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(Some(OWN_PR)));
    let handler = handler
        .with_branch_verifier(StubBranchVerifier::ok(&own_branch(&execution_id)))
        .with_feature_flags(flags_on(workspace.path()));
    assert_eq!(
        handler.sweep_pending_finalizes(0).await,
        PendingFinalizeSweepOutcome::default(),
        "a declaration newer than the cutoff is not swept"
    );
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());

    let outcome = handler.sweep_pending_finalizes(i64::MAX).await;
    assert_eq!(outcome.finalized, 1, "{outcome:?}");
    assert_eq!(task_row(&db, &chore_id).pr_url.as_deref(), Some(OWN_PR));
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert_eq!(
        handler.sweep_pending_finalizes(i64::MAX).await,
        PendingFinalizeSweepOutcome::default(),
        "a terminal execution is not swept again"
    );
}

/// A failed `no-changes-needed` terminal write at the Stop boundary is
/// retained and retried by the sweep with no further worker event.
#[tokio::test]
async fn failed_no_op_completion_write_is_retried_without_another_worker_event() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    crate::test_support::declare_no_changes_needed(&db, &execution_id);
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler.with_feature_flags(flags_on(workspace.path()));
    db.connect().unwrap().execute_batch(FAIL_TERMINAL_WRITE).unwrap();

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(outcome, StopOutcome::DbError, "{outcome:?}");
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());

    let pass = handler.sweep_pending_finalizes(i64::MAX).await;
    assert_eq!(pass.no_ops_still_pending, 1, "{pass:?}");
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());

    db.connect()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_finalize;")
        .unwrap();
    let pass = handler.sweep_pending_finalizes(i64::MAX).await;
    assert_eq!(pass.no_ops_finalized, 1, "{pass:?}");
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert_eq!(
        handler.sweep_pending_finalizes(i64::MAX).await,
        PendingFinalizeSweepOutcome::default(),
        "nothing left pending once the no-op committed"
    );
}

/// A turn-end arriving while a real finalize is in flight waits for it, then
/// runs against the resolved state instead of racing it.
#[tokio::test]
async fn turn_end_during_an_in_flight_finalize_waits_and_sees_the_terminal_state() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    stamp_delivered(&db, &execution_id);
    let gated = Arc::new(GatedPrDetector {
        inner: StubPrDetector::ok(Some(OWN_PR)),
        entered: Default::default(),
        release: tokio::sync::Notify::new(),
    });
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), gated.clone());
    let handler = Arc::new(handler.with_branch_verifier(StubBranchVerifier::ok(&own_branch(&execution_id))));

    let finalize = tokio::spawn({
        let handler = Arc::clone(&handler);
        let execution_id = execution_id.clone();
        async move {
            handler
                .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::Delivered)
                .await
        }
    });
    while gated.entered.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let stop = tokio::spawn({
        let handler = Arc::clone(&handler);
        let execution_id = execution_id.clone();
        async move { handler.on_stop(&execution_id).await }
    });
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(!stop.is_finished(), "the turn-end must wait for the in-flight finalize");
    assert_eq!(
        gated.entered.load(Ordering::SeqCst),
        1,
        "the turn-end ran inside the finalize"
    );

    gated.release.notify_one();
    let finalized = finalize.await.unwrap();
    assert!(matches!(finalized, StopOutcome::PrDetected { .. }), "{finalized:?}");
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(10), stop)
        .await
        .expect("turn-end handled once the finalize resolves")
        .unwrap();
    assert_eq!(stopped, StopOutcome::AlreadyTerminal);
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "released exactly once"
    );
}
