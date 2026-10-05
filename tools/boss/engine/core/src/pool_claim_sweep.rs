//! Periodic reconciler that releases worker-pool claims that have
//! outlived their execution.
//!
//! ## The leak this closes
//!
//! A [`crate::coordinator::WorkerPool`] slot is claimed at dispatch
//! (`drain_ready_queue` / `force_dispatch` → `claim_worker`) and, for a
//! pane-spawned run, its release is DEFERRED to
//! [`crate::app::ServerState::release_worker_pane`], which frees the
//! slot only when the macOS app tears the libghostty pane down. Every
//! other release path keys off a *live* worker:
//!
//! * completion (`force_release` / `force_stop_execution` /
//!   `finalize_pr_transition` / `finalize_automation_triage`) only frees
//!   the slot as a side effect of `release_worker_pane`, and only when
//!   the run→slot mapping is still registered;
//! * the dead-pid and stale-worker sweeps iterate
//!   [`LiveWorkerStateRegistry`] and derive the slot to release from a
//!   *live-state entry*;
//! * transient-recovery iterates the same registry but routes teardown
//!   through [`crate::app::ServerState::release_worker_pane`], so the
//!   pool claim is handed back only when the app confirms the pane is
//!   gone.
//!
//! Nothing iterates the pool's OWN claimed slots. So a slot claimed by
//! an execution that reached a terminal state WITHOUT a live pane —
//! a mid-spawn cancel (claim taken, no slot registered yet), a
//! `finalize_pr_transition` DB-error early-return, a teardown that
//! dropped the run→slot mapping but not the pool claim, or a
//! `bossctl agents stop` that released the cube lease but not the
//! claim — is released by NOTHING. The claim outlives its execution
//! forever. Once all [`MAX_AUTOMATION_POOL_SIZE`] automation slots leak
//! this way, `claim_worker` returns `None` for every automation
//! dispatch and the whole automation subsystem is wedged with no
//! self-healing path short of an engine restart.
//!
//! ## Algorithm
//!
//! For each pool (main and automation), snapshot the pool's own claimed
//! slots via [`WorkerPool::claims`] and, for each `(worker_id,
//! execution_id)`:
//!
//! 1. If a [`LiveWorkerStateRegistry`] entry still backs the claim (a
//!    live `run_id == execution_id`), SKIP — a live pane owns the slot
//!    and the completion / dead-pid / stale-worker paths own its
//!    teardown. Releasing it here would let a fresh dispatch hit
//!    `AttachWorkerPane` `SlotBusy` against a pane that is still up.
//!    Viewer attach can reject an occupied slot via AttachWorkerPane/SlotBusy;
//!    viewer teardown uses DetachWorkerPane. Neither viewer state nor a
//!    missing live-state mapping proves that a tmux worker has exited.
//! 2. Look up the execution. On a DB error, SKIP this pass (conservative
//!    — a transient error is not proof the row is gone).
//! 3. If the execution is NOT terminal, SKIP — the slot is legitimately
//!    held (claimed at dispatch, spawn in flight, or a live run).
//! 4. If the execution terminated within the last [`LEAK_GRACE_SECS`],
//!    SKIP — a legitimate teardown (e.g. `run_execution`'s tail, which
//!    releases the pool slot *unconditionally* after a mid-spawn cancel)
//!    may still be in flight, and racing it could double-release a slot
//!    a fresh dispatch has just re-claimed. The reconciler is a backstop
//!    for claims stuck for a while, not the happy path.
//! 5. Terminal execution + no live pane + past the grace = a leaked
//!    claim. Retry token-verified teardown of any recorded tmux identity.
//!    Retain unconfirmed claims and raise attention after three passes. Confirm its
//!    app viewer is detached (inventory once per pass, retain and retry
//!    when disconnected or unconfirmed), then release it via a
//!    compare-and-release
//!    ([`ExecutionCoordinator::release_pool_claim_if_execution`]) so a
//!    re-claim race can't yank a fresh, live claim, then emit a
//!    `pool_claim_reconcile` dispatch event and kick the scheduler.
//!
//! ## Why the live-state cross-check is sound
//!
//! On a CONFIRMED teardown, `release_worker_pane` frees the pool slot
//! BEFORE it drops the live-state entry (`app.rs`: `release_worker_and_kick`
//! then `live_worker_states.release_slot`). So a normal teardown's
//! observable states are "claimed + live entry" → "free + live entry" →
//! "free + no entry" — never "claimed + no entry" on that path.
//!
//! But an UNCONFIRMED teardown deliberately produces "claimed + no live
//! entry": when tmux process teardown or app viewer detach cannot be
//! confirmed (a missing session, timed-out request, or unexpected response),
//! `release_worker_pane` holds the pool claim instead of releasing it
//! (see the `sweep_owns_handback` branch there) while still dropping the
//! live-state entry unconditionally just below. This module is exactly
//! how that state gets resolved. Three producers currently yield this
//! shape:
//!
//! * a rejected viewer attachment in `coordinator/run.rs` (`hold_slot_busy`);
//! * an unconfirmed teardown in `release_worker_pane` (`sweep_owns_handback`);
//! * `TransientRecoveryReaper::reap_worker` dropping the live-state entry
//!   for a claim `release_worker_pane` never held or released, when
//!   transient-recovery finds no run→slot mapping. That path is reached
//!   only after `request_resume_execution` / `mark_execution_orphaned`
//!   has already terminalized the execution, so it is terminal-execution-
//!   only for the same reason as the other two.
//!
//! A "claimed + no live entry" slot is therefore not proof the pane is
//! gone — it may still be genuinely up, which is precisely what step 1
//! above is written to avoid racing ("Releasing it here would let a
//! fresh dispatch hit `AttachWorkerPane` `SlotBusy` against a pane that
//! is still up"). The grace period does not prove viewer teardown. Before
//! handback this sweep inventories app viewers and confirms the claimed
//! run's detach. A disconnected app leaves the claim pending until a later
//! pass can confirm it; registration also reconciles stale viewers.
//!
//! The tmux adoption and husk sweeps independently reconcile physical
//! sessions by durable spawn identity; releasing a claim is not proof that
//! a session or viewer has disappeared.
//!
//! ## Cadence
//!
//! Runs every [`DEFAULT_INTERVAL`] and fires once immediately on boot
//! (same pattern as [`crate::dead_pid_sweep`]) so a pool left wedged by
//! a crash self-heals at engine startup without an operator restart.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// How often the pool-claim reconciler runs. 60s mirrors the dead-pid
/// and stale-worker sweeps — fast enough that a leaked automation slot
/// is reclaimed within a minute, slow enough to be negligible overhead.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Grace period after an execution's `finished_at` during which a still-
/// claimed slot is left alone. A terminal execution's pool slot is
/// normally freed within milliseconds (completion's `release_worker_pane`
/// or `run_execution`'s unconditional tail release after a mid-spawn
/// cancel). The reconciler only steps in once a claim has outlived its
/// execution by longer than this — so it never races the happy-path
/// teardown and double-releases a slot a fresh dispatch just re-claimed.
/// A terminal execution with no parseable `finished_at` (a data anomaly —
/// every terminal path stamps it) is treated as past the grace.
pub const LEAK_GRACE_SECS: i64 = 60;

/// Confirms process teardown and viewer removal before claims can be reused.
///
/// Process teardown must verify durable spawn identity and clear it on success.
/// Viewer implementations inventory hosted panes once per call and detach every
/// matching occupant under a single lock hold.
#[async_trait::async_trait]
pub trait WorkerViewerDetach: Send + Sync {
    async fn confirm_process_torn_down(&self, execution_id: &str) -> Result<(), String>;

    async fn confirm_viewers_detached(&self, run_ids: &[String]) -> Vec<Result<(), String>>;
}

pub(crate) const TEARDOWN_ATTENTION_KIND: &str = "pool_claim_teardown_pending";

/// Consecutive failed confirmations, retained across sweep passes.
#[derive(Default)]
pub struct TeardownRetries {
    failures: HashMap<String, usize>,
}

impl TeardownRetries {
    fn failed(&mut self, db: &WorkDb, execution_id: &str, reason: &str) {
        tracing::warn!(
            execution_id,
            reason,
            "pool-claim sweep: teardown unconfirmed; retaining claim for retry"
        );
        let count = self.failures.entry(execution_id.to_owned()).or_default();
        *count = count.saturating_add(1);
        if *count < 3 {
            return;
        }
        match db.list_attention_items(execution_id) {
            Ok(items)
                if items
                    .iter()
                    .any(|item| item.kind == TEARDOWN_ATTENTION_KIND && item.status == "open") =>
            {
                return;
            }
            Ok(_) => {}
            Err(err) => {
                tracing::error!(execution_id, %err, "could not inspect pool teardown attention");
                return;
            }
        }
        if let Err(err) = db.create_attention_item(boss_protocol::CreateAttentionItemInput {
            execution_id: Some(execution_id.to_owned()),
            work_item_id: None,
            kind: TEARDOWN_ATTENTION_KIND.to_owned(),
            status: None,
            title: "Worker pool slot retained: teardown remains unconfirmed".to_owned(),
            body_markdown: format!("Teardown failed on {count} sweep passes. The pool slot remains held and teardown will be retried. Latest failure: {reason}"),
            resolved_at: None,
        }) {
            tracing::error!(execution_id, %err, "could not file pool teardown attention");
        }
    }

    async fn reconcile(&mut self, db: &WorkDb, coordinator: &ExecutionCoordinator) {
        let mut claimed = HashSet::new();
        for pool in [
            coordinator.worker_pool(),
            coordinator.automation_worker_pool(),
            coordinator.review_worker_pool(),
        ] {
            claimed.extend(pool.claims().await.into_iter().map(|claim| claim.execution_id));
        }
        self.failures.retain(|execution_id, _| claimed.contains(execution_id));
        // Durable open items are the retry queue, even after restart or
        // another teardown path has removed the in-memory claim.
        let items = match db.list_open_attention_items_of_kind(TEARDOWN_ATTENTION_KIND) {
            Ok(items) => items,
            Err(err) => {
                tracing::error!(%err, "could not inspect pool teardown attention");
                return;
            }
        };
        for item in items {
            if let Some(execution_id) = item.execution_id
                && !claimed.contains(&execution_id)
                && let Err(err) = db.resolve_attention_kind_for_execution(&execution_id, TEARDOWN_ATTENTION_KIND)
            {
                tracing::error!(execution_id, %err, "could not resolve pool teardown attention; retrying next pass");
            }
        }
    }
}

/// Counts from one sweep pass; logged at `info` when any claim was
/// released.
#[derive(Debug, Default, bon::Builder)]
pub struct PoolClaimSweepOutcome {
    /// Leaked claims (terminal execution, no live pane) that were freed.
    pub released: usize,
    /// Claims left alone because a live worker pane still backs them.
    pub live_backed_skipped: usize,
    /// Claims left alone because the execution is still non-terminal.
    pub non_terminal_skipped: usize,
    /// Terminal claims left alone because the execution terminated within
    /// [`LEAK_GRACE_SECS`] — a legitimate teardown may still be in flight.
    pub grace_skipped: usize,
    /// Claims skipped this pass because the execution lookup failed
    /// (conservative — retried next pass).
    pub lookup_failed_skipped: usize,
    /// Claims that lost the compare-and-release race (freed or re-claimed
    /// by a live execution between snapshot and release). Benign.
    pub race_skipped: usize,
    /// Unconfirmed viewer detach; the retained claim is retried next pass.
    #[builder(default)]
    pub viewer_detach_pending: usize,
    /// Claims retained because durable tmux identity is still recorded,
    /// so process teardown has not been confirmed.
    #[builder(default)]
    pub process_teardown_pending: usize,
}

impl crate::sweep_loop::SweepOutcome for PoolClaimSweepOutcome {
    fn has_activity(&self) -> bool {
        self.released > 0
    }

    fn log(&self) {
        tracing::info!(
            released = self.released,
            live_backed_skipped = self.live_backed_skipped,
            non_terminal_skipped = self.non_terminal_skipped,
            grace_skipped = self.grace_skipped,
            race_skipped = self.race_skipped,
            "pool-claim sweep: released leaked worker-pool claim(s)",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a pool wedged before an engine restart
/// self-heals at boot without waiting for the first interval.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
    viewers: Arc<dyn WorkerViewerDetach>,
) -> tokio::task::JoinHandle<()> {
    let retries = Arc::new(tokio::sync::Mutex::new(TeardownRetries::default()));
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let retries = Arc::clone(&retries);
        let viewers = Arc::clone(&viewers);
        let work_db = Arc::clone(&work_db);
        let live_states = Arc::clone(&live_states);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        async move {
            run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                coordinator.clone(),
                dispatch_events.as_ref(),
                viewers.as_ref(),
                &mut *retries.lock().await,
            )
            .await
        }
    })
}

/// Run a single pool-claim reconciliation pass over both pools. Returns
/// a summary of what happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because releasing a claim kicks the
/// scheduler, which spawns a task that holds a reference.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    viewers: &dyn WorkerViewerDetach,
    retries: &mut TeardownRetries,
) -> PoolClaimSweepOutcome {
    let mut outcome = PoolClaimSweepOutcome::default();

    let now_epoch_secs = boss_engine_utils::epoch_time::now_epoch_secs();
    let grace_cutoff = now_epoch_secs - LEAK_GRACE_SECS;

    // Executions that currently have a live worker pane. `run_id` on a
    // live-state entry IS the execution id (see dead_pid_sweep). A claim
    // whose execution is in this set is still owned by a live pane and
    // its teardown path — leave it alone.
    let live_run_ids: HashSet<String> = live_states.snapshot().into_iter().map(|state| state.run_id).collect();

    struct LeakedClaim {
        worker_id: String,
        execution_id: String,
        work_item_id: String,
        execution_status: String,
        pool_name: &'static str,
    }
    let mut leaked = Vec::new();

    for (pool, pool_name) in [
        (coordinator.worker_pool(), "main"),
        (coordinator.automation_worker_pool(), "automation"),
        (coordinator.review_worker_pool(), "review"),
    ] {
        for claim in pool.claims().await {
            // A live pane still backs this slot — the completion /
            // dead-pid / stale-worker paths own the release. Releasing
            // here would race a pane that may still be physically up.
            if live_run_ids.contains(&claim.execution_id) {
                outcome.live_backed_skipped += 1;
                continue;
            }

            let execution = match work_db.get_execution(&claim.execution_id) {
                Ok(execution) => execution,
                Err(err) => {
                    // A transient DB error is not proof the row is gone.
                    // Skip and retry next pass rather than free a
                    // possibly-live claim. (Mirrors dead_pid_sweep's
                    // conservative skip.)
                    tracing::warn!(
                        worker_id = %claim.worker_id,
                        execution_id = %claim.execution_id,
                        pool = pool_name,
                        ?err,
                        "pool-claim sweep: failed to look up claimed execution; skipping this pass",
                    );
                    outcome.lookup_failed_skipped += 1;
                    continue;
                }
            };

            if !execution.status.is_terminal() {
                // Legitimately held: claimed at dispatch with the spawn
                // still in flight, or a live run. Not our job.
                outcome.non_terminal_skipped += 1;
                continue;
            }

            // Grace guard: a freshly-terminalized claim may still be
            // mid-teardown by the path that owns it (e.g. run_execution's
            // unconditional tail release after a mid-spawn cancel).
            // Release only once it has been stuck past the grace, so we
            // never race the happy path. A missing/unparseable
            // finished_at (data anomaly — every terminal path stamps it)
            // falls through as past-grace.
            let finished_epoch = execution.finished_epoch();
            if matches!(finished_epoch, Some(t) if t > grace_cutoff) {
                outcome.grace_skipped += 1;
                continue;
            }

            match work_db.tmux_identity_for_execution(&claim.execution_id) {
                Ok(Some(_)) => {
                    if let Err(reason) = viewers.confirm_process_torn_down(&claim.execution_id).await {
                        retries.failed(work_db, &claim.execution_id, &reason);
                        outcome.process_teardown_pending += 1;
                        continue;
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        worker_id = %claim.worker_id,
                        execution_id = %claim.execution_id,
                        pool = pool_name,
                        ?err,
                        "pool-claim sweep: failed to look up tmux identity; skipping this pass",
                    );
                    outcome.lookup_failed_skipped += 1;
                    continue;
                }
            }

            tracing::warn!(
                worker_id = %claim.worker_id,
                execution_id = %claim.execution_id,
                pool = pool_name,
                execution_status = %execution.status,
                "pool-claim sweep: slot claimed by terminal execution with no live worker pane; \
                 confirming viewer teardown before releasing leaked claim",
            );

            leaked.push(LeakedClaim {
                worker_id: claim.worker_id,
                execution_id: claim.execution_id,
                work_item_id: execution.work_item_id,
                execution_status: execution.status.to_string(),
                pool_name,
            });
        }
    }

    let run_ids: Vec<String> = leaked.iter().map(|claim| claim.execution_id.clone()).collect();
    let mut detach_results = viewers.confirm_viewers_detached(&run_ids).await.into_iter();
    for claim in leaked {
        let detach = detach_results
            .next()
            .unwrap_or_else(|| Err("viewer detach result missing".into()));
        if let Err(reason) = detach {
            tracing::warn!(execution_id = %claim.execution_id, %reason,
                "pool-claim sweep: viewer detach pending; retaining claim for retry");
            retries.failed(work_db, &claim.execution_id, &reason);
            outcome.viewer_detach_pending += 1;
            continue;
        }

        let released = coordinator
            .release_pool_claim_if_execution(&claim.worker_id, &claim.execution_id)
            .await;

        if !released {
            // Lost the compare-and-release race: the slot was freed
            // or re-claimed by a live execution between the snapshot
            // and now. Benign — nothing to do.
            outcome.race_skipped += 1;
            continue;
        }

        outcome.released += 1;
        dispatch_events
            .emit(
                DispatchEvent::new(Stage::PoolClaimReconcile, Outcome::Ok, &claim.execution_id)
                    .with_work_item(&claim.work_item_id)
                    .with_worker(&claim.worker_id)
                    .with_details(serde_json::json!({
                        "pool": claim.pool_name,
                        "worker_id": claim.worker_id,
                        "execution_status": claim.execution_status,
                    })),
            )
            .await;
    }

    retries.reconcile(work_db, &coordinator).await;
    outcome
}

#[cfg(test)]
mod tests;
