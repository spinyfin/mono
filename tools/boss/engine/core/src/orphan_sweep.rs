//! Periodic reconciler that re-dispatches `active` work items with no
//! live execution — the post-crash "orphaned-in-Doing" fix.
//!
//! After an engine crash, work items that were `active` at the moment
//! of the crash stay `active` indefinitely if their executions were
//! classified as `Unknown` by the startup reconciler (no cube probe
//! signal either way). Without this module those items sit in the
//! kanban Doing column forever until a human manually runs
//! `bossctl work start <id>`.
//!
//! The sweep runs every 60 seconds and fires once immediately on
//! engine boot (same startup-sweep pattern as the merge poller). Each
//! pass:
//!
//! 1. Checks whether the worker pool has at least one idle slot; if
//!    not, returns early — a `ready` execution created now would just
//!    queue behind the full pool and can wait for the next sweep.
//! 2. Queries `active` work items whose `updated_at` is older than
//!    [`ORPHAN_MIN_AGE_SECS`] and that have no `ready`, `running` or
//!    `waiting_human` execution. Both live statuses describe a worker the
//!    engine dispatched and has not concluded — one working, one parked on
//!    a human — and either may have released its pool slot without being
//!    dead, so neither may be treated as orphaned. Deciding that a live row
//!    is actually a corpse belongs to the death sweeps (`dead_pane_sweep`,
//!    `husk_pane_sweep`, `lost_workspace_sweep`, `dead_pid_sweep`,
//!    `spawn_ack_sweep`); this sweep picks the item up on the pass after
//!    one of them reconciles it to `orphaned`/`abandoned`.
//! 3. For each candidate, checks whether its latest non-terminal
//!    execution (if any) is claimed by a live worker slot. If it is,
//!    the execution is genuinely live and the candidate is skipped.
//!    As a defense-in-depth guard, any candidate whose live execution is
//!    still in a live status at this point is also skipped unconditionally.
//! 4. Applies the **durable-process guard**: probes the pid recorded on the
//!    item's most recent local run ([`crate::durable_liveness`]) and refuses
//!    to redispatch while that process is alive, then hands the contradiction
//!    to [`crate::worker_readoption`] to be resolved. Every guard above this
//!    one reads engine bookkeeping, which is exactly what is wrong in the
//!    failure this guards — see the comment at the call site.
//! 5. Only once both liveness guards above have passed does the sweep act on
//!    the churn guard it evaluated earlier in the pass: if the work item has
//!    already had [`ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD`] terminal
//!    executions in the last [`ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS`],
//!    it is skipped, a warning is logged, and the item is bounced to Backlog
//!    via [`crate::work::WorkDb::bounce_churn_guard_parked_to_backlog`] (the
//!    same `dispatch_failed_reason` surface a pre-spawn dispatch failure
//!    uses) so the kanban board shows the park instead of the card sitting
//!    in Doing looking idle — see
//!    `docs/designs/dispatch-halt-state-vs-attention-items.md`. The bounce is
//!    deliberately sequenced *after* steps 3 and 4: those are the only
//!    checks that can tell a churn-tripped row apart from a row whose
//!    previous worker process is still alive (a live-but-untracked worker
//!    tends to also produce the terminal-execution churn that trips this
//!    guard), and bouncing first would demote a row to Backlog with a
//!    failure banner while its previous worker is still editing the
//!    workspace. Auto-clears once [`crate::dispatch_failure_recovery_sweep`]
//!    retries it after its cooldown: that sweep recognises a
//!    `CHURN_GUARD_DISPATCH_FAILED_REASON` row and applies *this* guard's
//!    own threshold/window to it (not its own looser 5-in-24h one), so the
//!    3-in-1h contract carries over unchanged rather than being weakened by
//!    the representation change. Also clears immediately on an explicit
//!    `bossctl work start` / kanban drag-to-Doing, either of which bypasses
//!    the guard entirely.
//! 6. Calls [`WorkDb::request_execution_with_live_check`] (the same
//!    path `bossctl work start` uses) to mark the stale execution
//!    `abandoned` and insert a fresh `ready` execution, then kicks
//!    the coordinator's scheduler.
//! 7. Emits an [`Stage::OrphanActiveRedispatch`] dispatch event so
//!    the redispatch is visible in `bossctl dispatch tail`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{ExecutionKind, ExecutionStatus, RequestExecutionInput};

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::{ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD, ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS, WorkDb};
use crate::worker_readoption::LiveWorkerConvergence;

/// Minimum age of `tasks.updated_at` before an active work item with
/// no live execution is treated as an orphan. Guards against racing a
/// fresh `todo → active` transition whose worker is still spinning up
/// but hasn't committed `run_started` yet.
pub const ORPHAN_MIN_AGE_SECS: i64 = 90;

/// Counts from one pass of the sweep; logged at `info` when non-zero.
///
/// Carries `bon::Builder` per the repo's >5-field convention. Production
/// builds it with `Default::default()` and increments in place — the builder
/// exists so a future field cannot force every construction site (including
/// each test's assertions) to be rewritten.
#[derive(Debug, Default, bon::Builder)]
pub struct OrphanSweepOutcome {
    pub redispatched: usize,
    pub churn_skipped: usize,
    pub no_worker_skipped: usize,
    /// Items skipped because their live execution is in a live status
    /// (`running` or `waiting_human`). These should already be filtered by
    /// the DB query; a non-zero count here indicates a data-consistency gap
    /// worth investigating.
    pub live_execution_skipped: usize,
    /// Items skipped because their live execution is a `running` `pr_review`
    /// (an active reviewer pane). With the union-of-pools liveness fix this
    /// should never fire; a non-zero count here means the pool snapshot did
    /// not include the review pool — worth investigating.
    pub running_reviewer_skipped: usize,
    /// Items skipped because the OS says the row's previous worker process is
    /// STILL RUNNING, whatever the engine's own bookkeeping believes. Each one
    /// is a duplicate worker that was not spawned.
    ///
    /// A non-zero count is not a health signal on its own — it means the
    /// durable-pid guard did its job — but a *sustained* non-zero count means
    /// executions are being terminalized while their workers live, and the
    /// convergence path ([`crate::worker_readoption`]) should have re-adopted
    /// or reaped them by now. Look at the paired `live_worker_readopted` /
    /// `husk_pane_reconcile` events before assuming the guard alone is enough.
    pub live_process_skipped: usize,
}

impl crate::sweep_loop::SweepOutcome for OrphanSweepOutcome {
    fn has_activity(&self) -> bool {
        self.redispatched > 0
            || self.churn_skipped > 0
            || self.live_execution_skipped > 0
            || self.running_reviewer_skipped > 0
            || self.live_process_skipped > 0
    }

    fn log(&self) {
        tracing::info!(
            redispatched = self.redispatched,
            churn_skipped = self.churn_skipped,
            no_worker_skipped = self.no_worker_skipped,
            live_execution_skipped = self.live_execution_skipped,
            running_reviewer_skipped = self.running_reviewer_skipped,
            live_process_skipped = self.live_process_skipped,
            "orphan sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so post-crash orphans are resolved on
/// engine boot without waiting for the first interval.
///
/// `convergence` resolves the contradiction the durable-process guard
/// detects. Passing [`NoopLiveWorkerConvergence`] leaves the guard in place
/// (no duplicate worker is ever created) but never resolves the underlying
/// state, so production must pass the real `ServerState`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    convergence: Arc<dyn LiveWorkerConvergence>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        let convergence = Arc::clone(&convergence);
        async move {
            run_one_pass(
                work_db.as_ref(),
                coordinator,
                dispatch_events.as_ref(),
                convergence.as_ref(),
            )
            .await
        }
    })
}

/// Run a single orphan-active sweep pass. Returns a summary of what
/// happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because kicking the scheduler
/// requires `Arc<ExecutionCoordinator>` — the kick path spawns a
/// tokio task that holds a reference.
pub async fn run_one_pass(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    convergence: &dyn LiveWorkerConvergence,
) -> OrphanSweepOutcome {
    let mut outcome = OrphanSweepOutcome::default();

    // Fast-path: if no worker slot is free, newly-queued executions
    // would just pile up in `ready`. Skip the DB scan entirely.
    if !coordinator.worker_pool().has_idle_worker().await {
        outcome.no_worker_skipped = 1; // sentinel so callers know why we bailed
        return outcome;
    }

    // Snapshot of which execution ids are currently claimed by a live
    // worker slot across ALL pools (main, automation, review).  Built
    // once outside the per-item loop so all items in this pass see a
    // consistent view.
    //
    // Using only `worker_pool()` (the main pool) would miss executions
    // claimed in the review or automation pools — a `pr_review` reviewer
    // is claimed in `review_pool`, so a main-pool-only snapshot would
    // incorrectly treat it as dead and abandon it.
    let claimed: HashSet<String> = coordinator.all_claimed_execution_ids().await;

    let candidates = match work_db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS) {
        Ok(ids) => ids,
        Err(err) => {
            tracing::warn!(?err, "orphan sweep: failed to list candidates; skipping pass");
            return outcome;
        }
    };

    let now_epoch_secs: i64 = boss_engine_utils::epoch_time::now_epoch_secs();
    let churn_cutoff = now_epoch_secs - ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS;

    for work_item_id in candidates {
        // Churn guard: count terminal executions in the trailing window.
        // Deliberately read-only here — whether the threshold is tripped is
        // decided now (recorded in the dispatch-decision event below), but
        // the *mutating* bounce-to-Backlog is deferred until after the
        // live-execution guard and the durable-process guard, both further
        // down, have had a chance to skip first. Those two guards are the
        // only things in this loop that can tell a churn-tripped row apart
        // from a row whose previous worker process is still alive — the
        // 2026-07-28 storm shape that produces >= 3 terminal executions in
        // an hour is exactly the shape that also confuses engine bookkeeping
        // about liveness (`docs/investigations/worker-liveness-convergence-design-review.md`
        // sec 3.3), so the two are correlated, not independent. Bouncing
        // before those guards run would demote a row to Backlog with a
        // failure banner while its previous worker is still editing the
        // workspace, and `autostart = 0` means the engine would never pick
        // it back up on its own.
        let recent_terminal = match work_db.count_recent_terminal_executions(&work_item_id, churn_cutoff, None) {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "orphan sweep: failed to count recent terminal executions; skipping item",
                );
                continue;
            }
        };
        let churn_tripped = recent_terminal >= ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD;

        // Decision-point instrumentation (re-dispatch storm visibility).
        //
        // This sweep is the prime recurring re-dispatcher, so when a
        // candidate already has a *live* execution (running /
        // waiting_human) we record exactly what the sweep keyed off
        // BEFORE acting: the live execution it found and whether the
        // worker pool still claims it. The two outcomes are the whole
        // diagnosis:
        //   - live_execution_claimed = true  → the guard in
        //     `request_execution_with_live_check` returns the live row
        //     and we skip (no redispatch). The event is the proof the
        //     storm was suppressed.
        //   - live_execution_claimed = false → the pool no longer claims
        //     the live run even though its DB status is non-terminal.
        //     THIS is the smoking gun for "scheduler re-fired despite a
        //     healthy live run" — previously invisible because the
        //     dispatch pipeline only records from `request_recorded` on.
        // Only emitted when a live execution exists; a candidate with no
        // live execution is a legitimate orphan whose redispatch is
        // already covered by `orphan_active_redispatch`.
        let live_execution = work_db
            .get_live_execution_for_work_item(&work_item_id, "")
            .ok()
            .flatten();
        if let Some(live) = &live_execution {
            let live_claimed = claimed.contains(&live.id);
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::DispatchDecision, Outcome::Ok, &live.id)
                        .with_work_item(&work_item_id)
                        .with_details(serde_json::json!({
                            "loop": "orphan_active_sweep",
                            "predicate": "tasks.status='active' AND no ready execution AND \
                                          updated_at age >= ORPHAN_MIN_AGE_SECS",
                            "live_execution_id": live.id,
                            "live_execution_status": live.status,
                            "live_execution_claimed": live_claimed,
                            "recent_terminal_executions": recent_terminal,
                        })),
                )
                .await;
        }

        // Defense-in-depth: never re-dispatch a work item that still has a
        // LIVE execution, even if the DB exclusion above somehow let it
        // through. `running` and `waiting_human` are the same fact — a
        // worker the engine dispatched and has not concluded — and either
        // may have released its pool slot without being dead. Abandoning
        // one would clobber a live in-flight workspace and put a duplicate
        // worker on the row.
        //
        // This guard is coupled to the candidate query above: both must
        // check the same two live statuses. If this check narrowed back
        // to `waiting_human` alone while the candidate query kept excluding
        // `running` too, every healthy `running` worker would fall straight
        // past this defense-in-depth check.
        if let Some(live) = &live_execution
            && live.status.is_live()
        {
            tracing::warn!(
                work_item_id = %work_item_id,
                execution_id = %live.id,
                status = %live.status,
                kind = %live.kind,
                "orphan sweep: candidate has a live execution; skipping \
                 (should have been excluded by DB query — investigate)",
            );
            if live.status == ExecutionStatus::Running
                && live.kind == ExecutionKind::PrReview
                && !claimed.contains(&live.id)
            {
                // Kept as a distinct counter: a live reviewer reaching
                // this guard while absent from `claimed` additionally
                // means the union-of-pools claim snapshot missed the
                // review pool. A non-zero `running_reviewer_skipped`
                // still means "investigate the pool union", exactly as
                // before.
                outcome.running_reviewer_skipped += 1;
            } else {
                outcome.live_execution_skipped += 1;
            }
            continue;
        }

        // Durable-process guard — the last thing checked before a duplicate
        // worker could be created, and the only check here that consults
        // something other than the engine's own opinion.
        //
        // Every guard above this point reads engine bookkeeping: the DB status
        // of the item's live execution, and `claimed` (the worker pool's claim
        // table). That is sound only while the bookkeeping is right. The
        // 2026-07-28 storm is what it looks like when it is wrong: six
        // executions were terminalized seconds after start while their `claude`
        // processes ran on for another nine minutes. Terminal status means no
        // "live execution" lookup finds them; a released pool claim means
        // `claimed` does not contain them; so every guard above says "orphan,
        // redispatch" — and a second, then third worker lands on a row the
        // first is still editing.
        //
        // `work_runs.shell_pid` outlives all of that. It is written when the
        // app reports the pane's shell pid, it survives an engine restart, and
        // it survives the execution going terminal. Probing it asks the OS
        // rather than the engine, which is the only way to break a tie where
        // the engine is the thing that is wrong.
        //
        // Skipping is not the end of the story: a row that is permanently
        // skipped is a row that never progresses. Convergence is
        // `worker_readoption`'s job (re-adopt or reap), and this guard's `Some`
        // branch is one of the two triggers that starts it. What this guard
        // guarantees on its own is narrower and is the point: no duplicate
        // worker is created while the previous one is alive.
        if let Some((blocking_execution_id, raw_process)) =
            crate::durable_liveness::probe_work_item_worker(work_db, &work_item_id, now_epoch_secs)
        {
            let blocking_execution = work_db.get_execution(&blocking_execution_id).ok();
            let blocking_status = blocking_execution
                .as_ref()
                .map(|exec| exec.status.to_string())
                .unwrap_or_else(|| "unknown".to_owned());

            // Corroborate a `Gone` verdict against the live-worker registry
            // before trusting it — the redispatch-guard half of the "live
            // workers false-reaped as orphaned" incident. `probe_work_item_worker`
            // reads the same fragile tracked-pid identity `dead_pane_sweep` and
            // `dead_pid_sweep` do; without this, the guard reads the same
            // wrong `Gone` verdict a false-reaping sweep just acted on and
            // fails open — letting a second worker dispatch onto a row whose
            // first worker is still running. `live_states` is `None` only
            // when no registry was wired up (a test, or a call site with no
            // live-state access), in which case the guard falls back to its
            // pre-fix behavior of trusting the bare probe.
            let live_states = coordinator.live_worker_states();
            let started_epoch = blocking_execution.as_ref().and_then(|exec| exec.started_epoch());
            let (process, corroboration) = match (live_states, started_epoch) {
                (Some(live), Some(started)) => crate::durable_liveness::corroborate_against_live_registry(
                    raw_process,
                    live,
                    &blocking_execution_id,
                    started,
                    now_epoch_secs,
                ),
                _ => (raw_process, None),
            };

            if process.is_alive() {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    blocking_execution_id = %blocking_execution_id,
                    blocking_status = %blocking_status,
                    shell_pid = process.shell_pid().unwrap_or(0),
                    corroborated = corroboration.is_some(),
                    "orphan sweep: refusing to redispatch — the row's previous worker process is still \
                     running. The engine's bookkeeping disagrees with the OS; the OS wins.",
                );
                dispatch_events
                    .emit(
                        DispatchEvent::new(
                            Stage::RedispatchBlockedLiveProcess,
                            Outcome::Skipped,
                            &blocking_execution_id,
                        )
                        .with_work_item(&work_item_id)
                        .with_details(serde_json::json!({
                            "loop": "orphan_active_sweep",
                            "blocking_execution_id": blocking_execution_id,
                            "blocking_execution_status": blocking_status,
                            "shell_pid": process.shell_pid(),
                            "recent_terminal_executions": recent_terminal,
                            "corroborated_alive": corroboration.is_some(),
                        })),
                    )
                    .await;
                outcome.live_process_skipped += 1;
                // Skipping alone would leave the row parked forever: never
                // redispatched (a live process blocks it) and never progressed
                // (the engine still believes its worker is dead). Hand the
                // contradiction to the convergence path, which re-adopts the run
                // if the terminal status was only an inference or reaps the
                // process if it was a decision. This is the trigger that covers
                // the case the hook fan-out cannot: a worker that is alive but
                // currently quiet — parked inside a long foreground build, say —
                // emits no hook to converge on, so without this the guard would
                // hold the row indefinitely.
                convergence
                    .converge_live_worker(&blocking_execution_id, "redispatch_guard")
                    .await;
                continue;
            }

            // The guard declined to block: instrumentation gap this closes.
            // Before this event, the guard was silent whenever it let a
            // redispatch through — diagnosing a wrongly-declined case (the
            // probe said `Gone`/`Unknown` when the worker was, or should have
            // been corroborated, alive) required cross-referencing this
            // sweep's trace lines against a different sweep's 45ms apart.
            // This makes the decision self-diagnosing from a single dispatch
            // tail: pid probed, probe result, and last-hook age.
            let last_event_at = live_states.and_then(|live| live.last_event_at_for_run(&blocking_execution_id));
            let last_event_age_secs = last_event_at
                .as_deref()
                .and_then(boss_engine_utils::iso8601::parse_iso8601_to_epoch)
                .map(|t| now_epoch_secs - t);
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::RedispatchGuardDeclined, Outcome::Ok, &blocking_execution_id)
                        .with_work_item(&work_item_id)
                        .with_details(serde_json::json!({
                            "loop": "orphan_active_sweep",
                            "blocking_execution_id": blocking_execution_id,
                            "blocking_execution_status": blocking_status,
                            "probe_result": process.reason(),
                            "shell_pid": process.shell_pid(),
                            "last_event_at": last_event_at,
                            "last_event_age_secs": last_event_age_secs,
                            "recent_terminal_executions": recent_terminal,
                        })),
                )
                .await;
        }

        // Now apply the churn guard's mutation. Both liveness guards above
        // have already run and neither skipped this candidate, so we know
        // (as well as this sweep ever can) that there is no live execution
        // and no live previous-worker process for this row — only now is it
        // safe to bounce it to Backlog with a failure banner.
        if churn_tripped {
            tracing::warn!(
                work_item_id = %work_item_id,
                recent_terminal,
                threshold = ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
                window_secs = ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS,
                "orphan sweep: churn guard tripped; skipping redispatch — human attention required",
            );
            let failing_ids = work_db
                .list_recent_terminal_execution_ids(&work_item_id, churn_cutoff, None)
                .unwrap_or_default();
            work_db.bounce_churn_guard_parked_to_backlog(
                &work_item_id,
                "orphan_sweep",
                recent_terminal,
                &failing_ids,
                "terminal executions",
            );
            outcome.churn_skipped += 1;
            continue;
        }

        // Request a fresh execution. The `is_live` closure treats an
        // execution as live only if a worker slot currently claims it.
        // A non-terminal execution that is NOT claimed means the worker
        // died without updating the DB — `request_execution_with_live_check`
        // will mark it `abandoned` and create a new `ready` row.
        let is_live = |exec_id: &str| claimed.contains(exec_id);
        let new_execution = match work_db.request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
            is_live,
        ) {
            Ok(exec) => exec,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "orphan sweep: failed to request execution; skipping item",
                );
                continue;
            }
        };

        // Only redispatch if we got a fresh ready execution. If the
        // existing non-terminal execution was live (claimed), the call
        // returns the existing execution with status != 'ready'.
        if new_execution.status != ExecutionStatus::Ready {
            continue;
        }

        tracing::info!(
            work_item_id = %work_item_id,
            execution_id = %new_execution.id,
            "orphan sweep: redispatching orphaned active work item",
        );

        dispatch_events
            .emit(
                DispatchEvent::new(Stage::OrphanActiveRedispatch, Outcome::Ok, &new_execution.id)
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
    use crate::coordinator::{ExecutionCoordinator, WorkerPool};
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::test_support::*;
    use crate::work::{ExecutionStatus, WorkDb};
    use crate::worker_readoption::NoopLiveWorkerConvergence;

    /// Stamp tasks.updated_at to 10 minutes ago so the age guard passes.
    fn make_old(db: &WorkDb, work_item_id: &str) {
        let old_epoch = boss_engine_utils::epoch_time::now_epoch_secs() - 600;
        db.force_updated_at_for_test(work_item_id, old_epoch).unwrap();
    }

    /// Like `make_coordinator` but also installs a review pool of `review_pool_size`.
    /// Returns both the coordinator and the review pool so the caller can claim slots.
    fn make_coordinator_with_review_pool(
        db: Arc<WorkDb>,
        pool_size: usize,
        review_pool_size: usize,
    ) -> (Arc<ExecutionCoordinator>, WorkerPool) {
        let review_pool = WorkerPool::new_review(review_pool_size);
        let mut coordinator =
            ExecutionCoordinator::new(db, WorkerPool::new(pool_size), Arc::new(NoopCube), Arc::new(NoopRunner));
        coordinator.set_review_pool(review_pool.clone());
        (Arc::new(coordinator), review_pool)
    }

    /// A pid guaranteed not to exist, so `kill(pid, 0)` returns `ESRCH`.
    /// Mirrors the same helper in `dead_pid_sweep`'s tests.
    fn dead_pid() -> i64 {
        4_194_303
    }

    /// Records every convergence trigger so a test can assert the sweep did
    /// not merely *skip* the row but handed the contradiction on to be
    /// resolved.
    #[derive(Default)]
    struct RecordingConvergence {
        converged: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl RecordingConvergence {
        fn converged(&self) -> Vec<(String, String)> {
            self.converged.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl LiveWorkerConvergence for RecordingConvergence {
        async fn converge_live_worker(&self, execution_id: &str, trigger: &str) {
            self.converged
                .lock()
                .unwrap()
                .push((execution_id.to_owned(), trigger.to_owned()));
        }
    }

    // ─── tests ──────────────────────────────────────────────────────────────

    /// **The 2026-07-28 duplicate-dispatch regression.**
    ///
    /// Reproduces the exact production shape: an execution the engine
    /// terminalized (`orphaned`) whose worker process is still running, on an
    /// item that every pre-existing guard reads as a legitimate orphan — its
    /// status is terminal so no live-execution lookup finds it, its pool claim
    /// was released so `claimed` does not contain it, and the churn window is
    /// empty. Before the durable-pid guard this redispatched, which is how one
    /// chore ended up with three concurrent workers.
    ///
    /// The invariant under test is the one the brief states: a redispatch
    /// attempt for a row whose prior process is still running must not produce
    /// a second live worker.
    #[tokio::test]
    async fn does_not_redispatch_over_a_still_running_worker_process() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        // Our own pid stands in for the worker's still-running shell.
        let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));
        db.mark_execution_orphaned(&execution_id, "spawn-ack timeout; worker presumed dead")
            .unwrap();
        // Age the item LAST: the execution/run writes above touch
        // `tasks.updated_at`, so ageing first would be undone by them and the
        // item would never clear ORPHAN_MIN_AGE_SECS.
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        // Nothing claimed: the pool released the slot when the execution was
        // terminalized, which is precisely why the sweep used to proceed.
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let convergence = RecordingConvergence::default();

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

        assert_eq!(
            outcome.redispatched, 0,
            "a second worker must never be dispatched onto a row whose first worker is alive",
        );
        assert_eq!(outcome.live_process_skipped, 1);

        // No new execution row at all — a `ready` row here would be dispatched
        // by the scheduler on its next drain, which is the duplicate.
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions.iter().all(|e| e.status != ExecutionStatus::Ready),
            "no fresh ready execution may be created while the prior process lives",
        );

        let events = sink.events().await;
        let blocked: Vec<_> = events
            .iter()
            .filter(|e| e.stage == "redispatch_blocked_live_process")
            .collect();
        assert_eq!(blocked.len(), 1, "the prevented duplicate must be observable");
        assert_eq!(blocked[0].outcome, "skipped");
        assert_eq!(
            blocked[0].details["blocking_execution_id"],
            serde_json::json!(execution_id)
        );
        assert_eq!(
            blocked[0].details["blocking_execution_status"],
            serde_json::json!("orphaned"),
            "the blocking row being TERMINAL is the whole point — that is what every other \
             guard reads as 'safe to redispatch'",
        );
        assert!(
            events.iter().all(|e| e.stage != "orphan_active_redispatch"),
            "no redispatch event may fire",
        );

        // Blocking alone would park the row forever; the contradiction must be
        // handed on for resolution.
        assert_eq!(
            convergence.converged(),
            vec![(execution_id, "redispatch_guard".to_owned())],
            "the guard must trigger convergence, not just decline",
        );
    }

    /// The guard must not become a permanent block. Once the worker process is
    /// genuinely gone, the same row redispatches exactly as before — this is
    /// what keeps the post-crash recovery the sweep exists for working.
    #[tokio::test]
    async fn redispatches_normally_once_the_prior_process_is_gone() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let convergence = RecordingConvergence::default();

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

        assert_eq!(
            outcome.redispatched, 1,
            "a dead prior process must not block recovery — that is what this sweep is for",
        );
        assert_eq!(outcome.live_process_skipped, 0);
        assert!(
            convergence.converged().is_empty(),
            "there is no contradiction to converge when the process is really gone",
        );
    }

    /// **The redispatch-guard half of the "live workers false-reaped as
    /// orphaned" incident.** The row's tracked pid probes dead — same as
    /// `redispatches_normally_once_the_prior_process_is_gone` — but the
    /// execution has emitted a hook well within the corroboration window.
    /// Without corroboration this guard reads the same wrong `Gone` verdict
    /// a false-reaping sweep just acted on and fails open, letting a second
    /// worker dispatch onto a row whose first worker is still running. With
    /// it, the guard must block exactly as if the probe had said `Alive`.
    #[tokio::test]
    async fn corroborated_activity_blocks_redispatch_despite_a_dead_probe() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.mark_execution_orphaned(&execution_id, "worker presumed dead")
            .unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
        live_states.register_spawn(1, &execution_id, "claude-opus-4-7", 424242, None);
        live_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        live_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );

        let mut coordinator =
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), Arc::new(NoopCube), Arc::new(NoopRunner));
        coordinator.set_live_worker_states(live_states);
        let coordinator = Arc::new(coordinator);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let convergence = RecordingConvergence::default();

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

        assert_eq!(
            outcome.redispatched, 0,
            "corroborated activity must block the redispatch even though the tracked pid probed dead",
        );
        assert_eq!(outcome.live_process_skipped, 1);

        let events = sink.events().await;
        let blocked: Vec<_> = events
            .iter()
            .filter(|e| e.stage == "redispatch_blocked_live_process")
            .collect();
        assert_eq!(blocked.len(), 1, "the corroborated block must be observable");
        assert_eq!(
            blocked[0].details["corroborated_alive"],
            serde_json::json!(true),
            "the event must record that corroboration (not a raw Alive probe) is what blocked this",
        );
        assert!(
            events.iter().all(|e| e.stage != "redispatch_guard_declined"),
            "a corroborated block is not a decline",
        );
        assert_eq!(
            convergence.converged(),
            vec![(execution_id, "redispatch_guard".to_owned())],
            "a corroborated block must still hand the contradiction on for resolution",
        );
    }

    /// The instrumentation gap this closes: before this event existed, the
    /// guard was silent whenever it declined to block — diagnosing a wrongly
    /// -declined redispatch required cross-referencing this sweep's trace
    /// lines against a different sweep's, 45ms apart. Every decline (with an
    /// actual probed pid to report on) must now be self-diagnosing from a
    /// single dispatch tail.
    #[tokio::test]
    async fn declined_guard_emits_instrumentation_event() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let convergence = RecordingConvergence::default();

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

        assert_eq!(
            outcome.redispatched, 1,
            "a genuinely gone process must not block recovery"
        );

        let events = sink.events().await;
        let declined: Vec<_> = events
            .iter()
            .filter(|e| e.stage == "redispatch_guard_declined")
            .collect();
        assert_eq!(declined.len(), 1, "the guard's decline must be observable, not silent");
        assert_eq!(declined[0].outcome, "ok");
        assert_eq!(
            declined[0].details["blocking_execution_id"],
            serde_json::json!(execution_id)
        );
        assert_eq!(declined[0].details["probe_result"], serde_json::json!("process_gone"),);
        assert!(
            declined[0].details["shell_pid"].is_number(),
            "the probed pid must be carried for diagnosis: {:?}",
            declined[0].details,
        );
    }

    /// The acceptance criterion for the decline event: when a registry entry
    /// exists, the payload must carry `last_event_age_secs` so an operator
    /// reading `bossctl dispatch diagnose` can tell a correct decline (hook
    /// aged out of the corroboration window) from a wrong one (recent hook
    /// that should have blocked). The no-registry
    /// [`declined_guard_emits_instrumentation_event`] case leaves these null.
    #[tokio::test]
    async fn declined_guard_event_carries_last_hook_age() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
        live_states.register_spawn(1, &execution_id, "claude-opus-4-7", 424242, None);
        live_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        live_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );
        // Older than the corroboration window so the guard still declines
        // (a recent hook would block redispatch via corroboration instead).
        let seeded_age_secs = crate::durable_liveness::CORROBORATION_WINDOW_SECS + 90;
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        live_states.set_last_event_at_for_test(1, crate::live_worker_state::iso8601_utc(now - seeded_age_secs));

        let mut coordinator =
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), Arc::new(NoopCube), Arc::new(NoopRunner));
        coordinator.set_live_worker_states(live_states);
        let coordinator = Arc::new(coordinator);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let convergence = RecordingConvergence::default();

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

        assert_eq!(
            outcome.redispatched, 1,
            "a hook aged past the corroboration window must not block recovery"
        );

        let events = sink.events().await;
        let declined: Vec<_> = events
            .iter()
            .filter(|e| e.stage == "redispatch_guard_declined")
            .collect();
        assert_eq!(declined.len(), 1, "the guard's decline must be observable");
        let age = declined[0].details["last_event_age_secs"]
            .as_i64()
            .expect("last_event_age_secs must be a number when a registry hook exists");
        assert!(
            (age - seeded_age_secs).abs() <= 5,
            "last_event_age_secs ({age}) must roughly match the seeded age ({seeded_age_secs})",
        );
        assert!(
            declined[0].details["last_event_at"].is_string(),
            "last_event_at must also be present: {:?}",
            declined[0].details,
        );
    }

    /// A work item with no recorded worker process at all has nothing for the
    /// guard to decline — no instrumentation event may fire for it, or every
    /// ordinary redispatch of a never-dispatched item would emit noise.
    #[tokio::test]
    async fn no_recorded_pid_emits_no_decline_event() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);
        db.mark_execution_orphaned(&execution_id, "spawn produced no shell")
            .unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(outcome.redispatched, 1);
        let events = sink.events().await;
        assert!(
            events.iter().all(|e| e.stage != "redispatch_guard_declined"),
            "a work item with no recorded pid has nothing to decline",
        );
    }

    /// A worker that never reported a pid (mid-spawn, or a spawn that never
    /// produced a shell) must not be treated as alive. `Unknown` is not
    /// `Alive`: reading it as such would disable orphan recovery for every
    /// execution that dies before `UpdateWorkerShellPid`.
    #[tokio::test]
    async fn a_never_reported_pid_does_not_block_redispatch() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);
        db.mark_execution_orphaned(&execution_id, "spawn produced no shell")
            .unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(outcome.live_process_skipped, 0);
        assert_eq!(outcome.redispatched, 1);
    }

    /// Orphan with NO execution → gets redispatched; dispatch event emitted.
    #[tokio::test]
    async fn redispatches_active_item_with_no_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(outcome.redispatched, 1, "should have redispatched one item");

        let events = sink.events().await;
        assert_eq!(events.len(), 1, "expected exactly one dispatch event");
        assert_eq!(events[0].stage, "orphan_active_redispatch");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions.iter().any(|e| e.status == ExecutionStatus::Ready),
            "expected a ready execution after redispatch"
        );
    }

    /// Active item with a live execution claimed by a worker slot → no-op.
    #[tokio::test]
    async fn skips_item_with_live_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        // Insert a ready execution and claim it in the pool — this makes
        // the item appear "already queued" (no-candidate via DB query).
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution.id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        // With a `ready` execution the DB query filters the item out.
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(outcome.redispatched, 0);
        assert!(sink.events().await.is_empty());
    }

    /// All worker slots busy → sweep returns early without touching the DB.
    #[tokio::test]
    async fn no_redispatch_when_all_workers_busy() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker("dummy-exec-id", None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(outcome.redispatched, 0);
        assert_eq!(outcome.no_worker_skipped, 1);
        assert!(sink.events().await.is_empty());
    }

    /// Churn guard: item with ≥ threshold recent terminal executions is skipped.
    #[tokio::test]
    async fn churn_guard_skips_repeatedly_failing_item() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "orphaned", now_epoch - i)
                .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(outcome.churn_skipped, 1, "churn guard should have fired");
        assert_eq!(outcome.redispatched, 0);
        assert!(sink.events().await.is_empty(), "no event on churn skip");
    }

    /// The churn guard trip must be operator-visible on the board itself,
    /// not just in a trace WARN or an attention item nobody renders: the
    /// work item bounces to Backlog (`status = "todo"`, `autostart =
    /// false`) with `dispatch_failed_reason = "churn_guard"` and an
    /// explanatory `dispatch_failed_error` — the same surface
    /// `WorkDispatchFailureBanner` (macOS app) already renders for a
    /// pre-spawn dispatch failure. It resolves automatically the next time
    /// a dispatch attempt is made against the item — whether that's a
    /// later sweep pass once the window drains, or an explicit `bossctl
    /// work start` bypassing the guard.
    #[tokio::test]
    async fn churn_guard_trip_bounces_to_backlog_and_clears_on_retry() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "orphaned", now_epoch - i)
                .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;
        assert_eq!(outcome.churn_skipped, 1);

        // No attention item — the park is engine/dispatch state, not a
        // human-judgment question, so it never touches `work_attention_items`.
        let items = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert!(
            items
                .iter()
                .all(|i| i.kind != crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND),
            "the active-task churn park must not file a churn_guard_parked attention item; got: {items:?}"
        );

        let task = get_task(&db, &work_item_id);
        assert_eq!(task.status.as_str(), "todo", "bounced item returns to Backlog");
        assert!(!task.autostart, "autostart must be cleared so the park doesn't loop");
        assert_eq!(task.dispatch_failed_reason.as_deref(), Some("churn_guard"));
        assert!(
            task.dispatch_failed_error
                .as_deref()
                .is_some_and(|e| e.contains("bossctl work start")),
            "dispatch_failed_error should point at the manual bypass verb: {:?}",
            task.dispatch_failed_error
        );

        // Bypassing the guard (the `bossctl work start` path) clears the
        // bounce immediately, without needing another sweep pass.
        db.request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
            |_| false,
        )
        .unwrap();

        let task_after = get_task(&db, &work_item_id);
        assert!(
            task_after.dispatch_failed_reason.is_none(),
            "dispatch_failed_reason should clear on the next dispatch attempt"
        );
    }

    /// Regression: the churn guard must not bounce a row to Backlog while
    /// the row's previous worker process is still alive. Before the fix,
    /// the churn-guard bounce ran and mutated the row (`status = 'todo'`,
    /// `autostart = 0`, failure banner) *before* the durable-process guard
    /// ever got a chance to detect the live process, so a worker still
    /// editing the workspace would get its work item yanked out from under
    /// it. The durable-process guard must win: no bounce, status stays
    /// `active`, and the live-process path (not the churn path) is the one
    /// that fires.
    #[tokio::test]
    async fn churn_trip_does_not_bounce_while_prior_process_is_alive() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");

        // Enough terminal executions to trip the churn guard...
        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "orphaned", now_epoch - i)
                .unwrap();
        }
        // ...but the most recent run's shell_pid is still alive (our own
        // pid stands in for the still-running worker shell).
        let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));
        db.mark_execution_orphaned(&execution_id, "spawn-ack timeout; worker presumed dead")
            .unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let convergence = RecordingConvergence::default();

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &convergence).await;

        assert_eq!(
            outcome.live_process_skipped, 1,
            "the durable-process guard must fire before the churn guard's bounce"
        );
        assert_eq!(
            outcome.churn_skipped, 0,
            "the churn bounce must not run while a prior process is alive"
        );
        assert_eq!(outcome.redispatched, 0);

        let task = get_task(&db, &work_item_id);
        assert_eq!(
            task.status.as_str(),
            "active",
            "the row must not be bounced to Backlog while its prior worker is still alive"
        );
        assert!(
            task.dispatch_failed_reason.is_none(),
            "no churn-guard park while the process is alive"
        );
    }

    fn get_task(db: &WorkDb, work_item_id: &str) -> boss_protocol::Task {
        match db.get_work_item(work_item_id).unwrap() {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t,
            other => panic!("expected a task/chore work item, got {other:?}"),
        }
    }

    /// Recent-transition guard: freshly-activated item is skipped even with
    /// no execution, because its updated_at is within ORPHAN_MIN_AGE_SECS.
    #[tokio::test]
    async fn no_redispatch_for_recently_activated_item() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let _work_item_id = create_active_chore(&db, &product_id, "test chore");
        // Deliberately do NOT call make_old — item's updated_at is NOW.

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(outcome.redispatched, 0, "should skip recently activated item");
        assert!(sink.events().await.is_empty());
    }

    /// Regression: a waiting_human execution must never be abandoned and
    /// re-dispatched by the orphan sweep. The worker parks for human input
    /// and then exits (releasing its pool slot), so the execution is not
    /// claimed — but it is still alive and waiting for a response.
    ///
    /// Previously the sweep treated unclaimed + non-terminal as "dead worker"
    /// and double-dispatched a second worker onto the same row
    /// (exec_18b508391244f798_34 → exec_18b508565e3b6e30_39).
    #[tokio::test]
    async fn skips_item_with_waiting_human_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        // Create a ready execution then force it to waiting_human to simulate
        // a worker that parked for human input and then released its slot.
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        db.force_execution_status_for_test(&work_item_id, ExecutionStatus::WaitingHuman)
            .unwrap();

        let db = Arc::new(db);
        // Deliberately do NOT claim the execution — simulates the worker
        // process having exited after entering waiting_human.
        let coordinator = make_coordinator(db.clone(), 1);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(
            outcome.redispatched, 0,
            "sweep must not re-dispatch a waiting_human execution"
        );
        let events = sink.events().await;
        assert!(
            events.iter().all(|e| e.stage != "orphan_active_redispatch"),
            "no orphan_active_redispatch event should fire for waiting_human"
        );

        // The waiting_human execution must remain intact — not abandoned.
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions
                .iter()
                .any(|e| e.id == execution.id && e.status == ExecutionStatus::WaitingHuman),
            "waiting_human execution must not be abandoned by the sweep"
        );
    }

    /// The same protection for `running`, which is the status EVERY healthy
    /// pane worker sits in for its whole life — making this the common
    /// case, not an edge one.
    ///
    /// Deciding a live row is actually dead belongs to the death sweeps
    /// (`dead_pane_sweep`, `husk_pane_sweep`, `lost_workspace_sweep`,
    /// `dead_pid_sweep`, `spawn_ack_sweep`); this sweep picks the item up
    /// on the pass after one of them reconciles it to `orphaned`.
    #[tokio::test]
    async fn skips_item_with_running_worker_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        db.force_execution_status_for_test(&work_item_id, ExecutionStatus::Running)
            .unwrap();

        let db = Arc::new(db);
        // Deliberately unclaimed, the shape that made the pre-fix sweep
        // treat "unclaimed + non-terminal" as a dead worker.
        let coordinator = make_coordinator(db.clone(), 1);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(
            outcome.redispatched, 0,
            "sweep must not re-dispatch on top of a running worker"
        );
        assert!(
            !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
                .unwrap()
                .contains(&work_item_id),
            "a work item with a running execution must not be an orphan candidate at all"
        );
        let events = sink.events().await;
        assert!(
            events.iter().all(|e| e.stage != "orphan_active_redispatch"),
            "no orphan_active_redispatch event should fire for a running worker"
        );

        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions
                .iter()
                .any(|e| e.id == execution.id && e.status == ExecutionStatus::Running),
            "running execution must not be abandoned by the sweep"
        );
    }

    /// Regression: the sweep double-dispatched a second worker onto the same
    /// row when the live worker was a review-pool `pr_review` execution.
    ///
    /// A `running` `pr_review` execution is a live reviewer pane actively
    /// working (`RunWaitState::WorkerPaneAlive`). The reviewer is claimed
    /// in the REVIEW pool — not the MAIN pool. The old sweep only consulted
    /// `coordinator.worker_pool().claimed_execution_ids()` (the main pool),
    /// so a review-pool-claimed reviewer read as dead. The sweep would then
    /// abandon the live pr_review execution and re-dispatch a fresh
    /// chore_implementation on top of the already-pushed PR.
    ///
    /// The fix: `all_claimed_execution_ids()` unions all three pools. This
    /// test verifies the fix by claiming the pr_review execution in the
    /// review pool only (never the main pool) and asserting the sweep does
    /// not abandon it.
    #[tokio::test]
    async fn running_pr_review_in_review_pool_is_not_abandoned() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        // Create a pr_review execution and force it to `running` to simulate
        // a reviewer pane that was successfully spawned.
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        // Override kind to PrReview — the execution was created with the
        // default kind; we force the DB value directly so the sweep reads it.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET kind = 'pr_review', status = 'running' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();
        }

        let db = Arc::new(db);
        // Build a coordinator with a 1-slot main pool AND a 1-slot review pool.
        // Claim the pr_review execution in the REVIEW pool (not the main pool)
        // to simulate the production layout: main pool has an idle slot (so
        // the fast-path check passes), but the reviewer is live in review pool.
        let (coordinator, review_pool) = make_coordinator_with_review_pool(db.clone(), 1, 1);
        review_pool.claim_worker(&execution.id, None).await;
        // Main pool is idle — this is what previously triggered the bug:
        // has_idle_worker() = true (sweep proceeds), but the main-pool
        // claimed_execution_ids() didn't include the reviewer exec id.

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(
            outcome.redispatched, 0,
            "sweep must not re-dispatch when the pr_review execution is claimed in the review pool"
        );
        assert_eq!(
            outcome.running_reviewer_skipped, 0,
            "defense-in-depth skip must not fire when pool union correctly identifies the reviewer as live"
        );
        let events = sink.events().await;
        assert!(
            events.iter().all(|e| e.stage != "orphan_active_redispatch"),
            "no orphan_active_redispatch event must fire for a live review-pool-claimed reviewer"
        );

        // The running pr_review execution must remain intact — not abandoned.
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions
                .iter()
                .any(|e| e.id == execution.id && e.status == ExecutionStatus::Running),
            "running pr_review execution must not be abandoned by the sweep"
        );
    }

    /// A live reviewer claimed in NO pool at all — the "pool union absent"
    /// scenario — must still survive the sweep.
    ///
    /// This is enforced one layer before the in-loop guard:
    /// `list_orphan_active_candidates` excludes every work item with a live
    /// (`running`/`waiting_human`) execution, so the item never reaches the
    /// in-loop guard and `running_reviewer_skipped` stays 0. The guard is
    /// retained as genuine defense-in-depth; the assertion below on the
    /// candidate list is what pins the mechanism, so a future change that
    /// re-admits live rows to the candidate set fails here rather than
    /// silently falling back on the guard.
    #[tokio::test]
    async fn running_pr_review_not_in_any_pool_survives_the_sweep() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET kind = 'pr_review', status = 'running' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();
        }

        let db = Arc::new(db);
        // Claim nothing in any pool — simulates the "pool union absent" scenario.
        let coordinator = make_coordinator(db.clone(), 1);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(
            outcome.redispatched, 0,
            "the sweep must not re-dispatch on top of a running pr_review execution"
        );
        assert!(
            !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
                .unwrap()
                .contains(&work_item_id),
            "a work item with a live execution must be excluded at the candidate query, before \
             the in-loop defense-in-depth guard is ever consulted"
        );
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions
                .iter()
                .any(|e| e.id == execution.id && e.status == ExecutionStatus::Running),
            "running pr_review execution must survive the sweep even when not in any pool"
        );
    }
}
