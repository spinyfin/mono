//! Periodic reconciler that re-enqueues work items whose dispatch
//! failed terminally *before* a worker ever spawned.
//!
//! `record_pre_start_failure` already retries a pre-spawn dispatch
//! failure (cube repo ensure, workspace lease, host selection, change
//! create, run start) with backoff — `PRE_START_RETRY_DELAYS`, ~65s
//! total across 3 retries. When those are exhausted,
//! `WorkDb::bounce_dispatch_failed_to_backlog` parks the work item in
//! Backlog (`autostart = 0`, `dispatch_failed_reason` stamped) and
//! raises a `WorkAttentionItem` quoting the underlying error. That is
//! deliberately terminal from the scheduler's point of view: as
//! `request_execution_in_tx_with_live_check` documents, "automatic
//! re-dispatch sweeps (reconcile/rescan) never reach a work item
//! bounced by `bounce_dispatch_failed_to_backlog`... a human must
//! retry deliberately" — no periodic sweep ever revisited a bounced
//! row. That is fine for a genuinely broken dispatch target (bad repo
//! config, cancelled work), but it strands a work item whose failure
//! was merely transient (a flaky host, a momentary cube error) with no
//! self-healing story: `orphan_active_redispatch` / `cube_lease_auto_reap`
//! already provide that for failures *after* `run_started`, but nothing
//! played that role pre-spawn (2026-07-03 T215 incident: a dispatch
//! failed pre-spawn, exhausted its retries, and sat parked for 45+
//! minutes with free worker slots available, until a human ran
//! `bossctl work start`).
//!
//! This sweep closes that gap. Every [`DISPATCH_FAILURE_RECOVERY_MIN_AGE_SECS`]
//! it looks for work items still bounced in Backlog and gives them
//! another shot via `WorkDb::reenable_and_request_execution_with_live_check`,
//! which re-enables autostart and requests a fresh execution atomically
//! in one transaction — the same round trip a human's own retry already
//! performs, but with no window where a `request_execution` failure can
//! strand the item with autostart flipped back on and no execution. A
//! churn guard
//! ([`DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_THRESHOLD`] terminal
//! executions inside [`DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_WINDOW_SECS`])
//! stops the automatic retries once a work item keeps failing. From
//! then on it stays parked for the human, and the attention item raised
//! at the original bounce remains the escalation; this sweep does not
//! raise a second one on every skip.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::RequestExecutionInput;

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::{
    DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_THRESHOLD, DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_WINDOW_SECS,
    DISPATCH_FAILURE_RECOVERY_MIN_AGE_SECS, WorkDb,
};

/// Counts from one pass of the sweep; logged at `info` when non-zero.
#[derive(Debug, Default)]
pub struct DispatchFailureRecoverySweepOutcome {
    pub redispatched: usize,
    pub churn_skipped: usize,
    pub no_worker_skipped: usize,
}

impl crate::sweep_loop::SweepOutcome for DispatchFailureRecoverySweepOutcome {
    fn has_activity(&self) -> bool {
        self.redispatched > 0 || self.churn_skipped > 0
    }

    fn log(&self) {
        tracing::info!(
            redispatched = self.redispatched,
            churn_skipped = self.churn_skipped,
            no_worker_skipped = self.no_worker_skipped,
            "dispatch-failure recovery sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so rows bounced while the engine was down
/// get a chance without waiting a full interval.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        async move { run_one_pass(work_db.as_ref(), coordinator.clone(), dispatch_events.as_ref()).await }
    })
}

/// Run a single dispatch-failure-recovery sweep pass. Returns a summary
/// of what happened; callers may log it.
pub async fn run_one_pass(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
) -> DispatchFailureRecoverySweepOutcome {
    let mut outcome = DispatchFailureRecoverySweepOutcome::default();

    // Fast-path: a fresh `ready` execution would just queue behind a
    // full pool, so skip the DB scan entirely when nothing is idle.
    if !coordinator.worker_pool().has_idle_worker().await {
        outcome.no_worker_skipped = 1;
        return outcome;
    }

    let candidates = match work_db.list_dispatch_failed_recovery_candidates(DISPATCH_FAILURE_RECOVERY_MIN_AGE_SECS) {
        Ok(ids) => ids,
        Err(err) => {
            tracing::warn!(
                ?err,
                "dispatch-failure recovery sweep: failed to list candidates; skipping pass"
            );
            return outcome;
        }
    };

    let now_epoch_secs: i64 = boss_engine_utils::epoch_time::now_epoch_secs();
    let churn_cutoff = now_epoch_secs - DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_WINDOW_SECS;

    // Snapshot of claimed execution ids across every pool, same shape
    // orphan_sweep uses, so a live execution is never mistaken for dead.
    let claimed = coordinator.all_claimed_execution_ids().await;

    for work_item_id in candidates {
        let recent_terminal = match work_db.count_recent_terminal_executions(&work_item_id, churn_cutoff) {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "dispatch-failure recovery sweep: failed to count recent terminal executions; skipping item",
                );
                continue;
            }
        };
        if recent_terminal >= DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_THRESHOLD {
            tracing::warn!(
                work_item_id = %work_item_id,
                recent_terminal,
                threshold = DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_THRESHOLD,
                window_secs = DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_WINDOW_SECS,
                "dispatch-failure recovery sweep: churn guard tripped; leaving parked for human attention",
            );
            outcome.churn_skipped += 1;
            continue;
        }

        let is_live = |exec_id: &str| claimed.contains(exec_id);
        let new_execution = match work_db.reenable_and_request_execution_with_live_check(
            &work_item_id,
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
            is_live,
        ) {
            Ok(Some(exec)) => exec,
            // Raced a human retry (or the row moved on) between listing
            // candidates and here — nothing left to do.
            Ok(None) => continue,
            Err(err) => {
                // The autostart re-enable and `request_execution` ran in
                // one transaction, so this rolled back together — the
                // item is left exactly as it was (autostart = 0,
                // dispatch_failed_reason still set), still a candidate
                // for the next pass, instead of stranded with
                // autostart = 1 and no execution.
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "dispatch-failure recovery sweep: failed to re-enqueue; leaving bounced for next pass",
                );
                continue;
            }
        };

        tracing::info!(
            work_item_id = %work_item_id,
            execution_id = %new_execution.id,
            "dispatch-failure recovery sweep: re-enqueuing work item after pre-spawn dispatch failure",
        );

        dispatch_events
            .emit(
                DispatchEvent::new(Stage::DispatchFailureRecoveryRedispatch, Outcome::Ok, &new_execution.id)
                    .with_work_item(&work_item_id)
                    .with_details(serde_json::json!({
                        "recent_terminal_executions": recent_terminal,
                    })),
            )
            .await;

        coordinator.kick();
        outcome.redispatched += 1;
    }

    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::test_support::*;
    use crate::work::{CreateChoreInput, ExecutionStatus, WorkDb};

    // `NoopCube` and `NoopRunner` come from `crate::test_support::*`.

    fn create_bounced_chore(db: &WorkDb, product_id: &str) -> String {
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("test chore")
                    .build(),
            )
            .unwrap();
        assert!(
            db.bounce_dispatch_failed_to_backlog(&chore.id, "cube_workspace_lease_failed", "boom")
                .unwrap(),
            "bounce must apply to a fresh todo/autostart chore"
        );
        chore.id
    }

    /// Stamp `dispatch_failed_at` to `age_secs` in the past so the
    /// cooldown gate passes (or not, for the "too recent" test).
    fn make_failure_old(db: &WorkDb, work_item_id: &str, age_secs: i64) {
        let old_epoch = boss_engine_utils::epoch_time::now_epoch_secs() - age_secs;
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET dispatch_failed_at = ?2 WHERE id = ?1",
            rusqlite::params![work_item_id, old_epoch.to_string()],
        )
        .unwrap();
    }

    fn get_task(db: &WorkDb, work_item_id: &str) -> boss_protocol::Task {
        match db.get_work_item(work_item_id).unwrap() {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t,
            other => panic!("expected a task/chore work item, got {other:?}"),
        }
    }

    /// A bounced item whose failure is older than the cooldown gets
    /// re-enqueued: `autostart` flips back on, `dispatch_failed_reason`
    /// clears, and a fresh `ready` execution appears.
    #[tokio::test]
    async fn redispatches_bounced_item_past_cooldown() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_bounced_chore(&db, &product_id);
        make_failure_old(&db, &work_item_id, DISPATCH_FAILURE_RECOVERY_MIN_AGE_SECS + 60);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.redispatched, 1, "should have re-enqueued the bounced item");

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "dispatch_failure_recovery_redispatch");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

        let task = get_task(&db, &work_item_id);
        assert!(task.autostart, "autostart must be re-enabled");
        assert!(
            task.dispatch_failed_reason.is_none(),
            "dispatch failure marker must clear"
        );

        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions.iter().any(|e| e.status == ExecutionStatus::Ready),
            "expected a fresh ready execution after re-enqueue"
        );
    }

    /// A bounce still inside the cooldown window is left alone.
    #[tokio::test]
    async fn no_redispatch_before_cooldown_elapses() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_bounced_chore(&db, &product_id);
        // Deliberately do NOT age dispatch_failed_at — it's "now".

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.redispatched, 0, "cooldown has not elapsed yet");
        assert!(sink.events().await.is_empty());

        let task = get_task(&db, &work_item_id);
        assert!(!task.autostart, "autostart must remain cleared during cooldown");
    }

    /// A task with no dispatch failure recorded is never touched.
    #[tokio::test]
    async fn ignores_healthy_todo_item() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("healthy chore")
                    .build(),
            )
            .unwrap();

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.redispatched, 0);
        assert!(sink.events().await.is_empty());
        let _ = chore;
    }

    /// Churn guard: a work item that keeps producing terminal executions
    /// is left parked for a human instead of being retried forever.
    #[tokio::test]
    async fn churn_guard_skips_repeatedly_failing_item() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_bounced_chore(&db, &product_id);
        make_failure_old(&db, &work_item_id, DISPATCH_FAILURE_RECOVERY_MIN_AGE_SECS + 60);

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        for i in 0..DISPATCH_FAILURE_RECOVERY_CHURN_GUARD_THRESHOLD {
            db.insert_terminal_execution_for_test(&work_item_id, "failed", now_epoch - i)
                .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.churn_skipped, 1, "churn guard should have fired");
        assert_eq!(outcome.redispatched, 0);
        assert!(sink.events().await.is_empty(), "no event on churn skip");

        let task = get_task(&db, &work_item_id);
        assert!(
            !task.autostart,
            "autostart must stay cleared once the churn guard trips"
        );
    }

    /// If `request_execution` fails *after* autostart has been
    /// re-enabled, the whole re-enable-and-request must roll back
    /// together: the item is left exactly as it was before the sweep
    /// touched it (autostart cleared, dispatch_failed_reason still
    /// set), so it remains a candidate for the next pass instead of
    /// being stranded with autostart on and no execution.
    #[tokio::test]
    async fn rolls_back_autostart_when_request_execution_fails_after_reenable() {
        use boss_protocol::AddDependencyInput;

        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_bounced_chore(&db, &product_id);
        make_failure_old(&db, &work_item_id, DISPATCH_FAILURE_RECOVERY_MIN_AGE_SECS + 60);

        // Gate the bounced chore on a still-incomplete prerequisite so
        // `request_execution_in_tx_with_live_check`'s gating check bails.
        let prereq = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("prereq chore")
                    .build(),
            )
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: work_item_id.clone(),
            prerequisite: prereq.id.clone(),
            relation: None,
        })
        .unwrap();

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(
            outcome.redispatched, 0,
            "gated request_execution must not count as redispatched"
        );
        assert!(
            sink.events().await.is_empty(),
            "no redispatch event on a rolled-back attempt"
        );

        let task = get_task(&db, &work_item_id);
        assert!(
            !task.autostart,
            "autostart re-enable must roll back alongside the failed request_execution"
        );
        assert!(
            task.dispatch_failed_reason.is_some(),
            "dispatch failure marker must still be set so the item remains a recovery candidate"
        );

        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            !executions.iter().any(|e| e.status == ExecutionStatus::Ready),
            "no fresh execution should have been created"
        );
    }

    /// All worker slots busy → sweep returns early without touching the DB.
    #[tokio::test]
    async fn no_redispatch_when_all_workers_busy() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_bounced_chore(&db, &product_id);
        make_failure_old(&db, &work_item_id, DISPATCH_FAILURE_RECOVERY_MIN_AGE_SECS + 60);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker("dummy-exec-id", None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.redispatched, 0);
        assert_eq!(outcome.no_worker_skipped, 1);
        assert!(sink.events().await.is_empty());
    }
}
