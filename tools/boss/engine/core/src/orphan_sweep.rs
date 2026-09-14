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
//! engine boot (same startup-sweep pattern as the merge poller). It is
//! also event-driven: [`spawn_event_subscriber`] subscribes to
//! [`Event::ExecutionTerminal`] and re-evaluates just the terminated
//! execution's work item immediately, so an orphan left behind by a
//! worker crash is usually redispatched within milliseconds instead of
//! waiting out the interval. The periodic sweep is kept unconditionally
//! as the backstop for whatever the best-effort bus drops. Each
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
//! 3. Applies the two **admission gates** every recurring redispatcher
//!    owes the rest of the engine, per candidate: whether the row's last
//!    run ended in a deliberate engine park (a `boss propose done
//!    --outcome blocked` declaration, or the auto-nudge breaker giving
//!    up — both terminalize the run as `abandoned` and leave an open
//!    attention item), and the global dispatch pause as
//!    [`ExecutionCoordinator::evaluate_dispatch_admission`] reports it.
//!    Neither gate stops the sweep from running or from evaluating: they
//!    stop it *minting an execution*, which is the only irreversible
//!    thing it does (`request_execution_with_live_check` marks the
//!    predecessor `abandoned`). Both fail closed — an admission state
//!    that cannot be established holds the row and logs, rather than
//!    defaulting to redispatch.
//! 4. For each candidate, checks whether its latest non-terminal
//!    execution (if any) is claimed by a live worker slot. If it is,
//!    the execution is genuinely live and the candidate is skipped.
//!    As a defense-in-depth guard, any candidate whose live execution is
//!    still in a live status at this point is also skipped unconditionally.
//! 5. Applies the **durable-process guard**: probes the pid recorded on the
//!    item's most recent local run ([`crate::durable_liveness`]) and refuses
//!    to redispatch while that process is alive, then hands the contradiction
//!    to [`crate::worker_readoption`] to be resolved. Every guard above this
//!    one reads engine bookkeeping, which is exactly what is wrong in the
//!    failure this guards — see the comment at the call site.
//! 6. Only once both liveness guards above have passed does the sweep act on
//!    the churn guard it evaluated earlier in the pass. The guard has two
//!    halves and either one trips it: [`ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD`]
//!    terminal executions inside the trailing
//!    [`ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS`] (fast churn), or
//!    [`ORPHAN_REDISPATCH_CHURN_GUARD_CONSECUTIVE_THRESHOLD`] *consecutive*
//!    unproductive terminal executions with no successful run in between,
//!    however long they took (slow churn, which no trailing window can
//!    catch — see that constant's docs). On a trip the item is skipped, a
//!    warning is logged, and the item is bounced to Backlog
//!    via [`crate::work::WorkDb::bounce_churn_guard_parked_to_backlog`] (the
//!    same `dispatch_failed_reason` surface a pre-spawn dispatch failure
//!    uses) so the kanban board shows the park instead of the card sitting
//!    in Doing looking idle — see
//!    `docs/designs/dispatch-halt-state-vs-attention-items.md`. The bounce is
//!    deliberately sequenced *after* steps 4 and 5: those are the only
//!    checks that can tell a churn-tripped row apart from a row whose
//!    previous worker process is still alive (a live-but-untracked worker
//!    tends to also produce the terminal-execution churn that trips this
//!    guard), and bouncing first would demote a row to Backlog with a
//!    failure banner while its previous worker is still editing the
//!    workspace. Auto-clears once [`crate::dispatch_failure_recovery_sweep`]
//!    retries it after its cooldown: that sweep recognises a
//!    `CHURN_GUARD_DISPATCH_FAILED_REASON` row and applies *this* guard's
//!    own thresholds to it — both halves, not its own looser 5-in-24h one —
//!    so the contract carries over unchanged rather than being weakened by
//!    the representation change (its 10-minute cooldown is shorter than any
//!    window, so without the consecutive half it would un-park a slow loop
//!    one cooldown at a time). Also clears immediately on an explicit
//!    `bossctl work start` / kanban drag-to-Doing, either of which bypasses
//!    the guard entirely.
//! 7. Calls [`WorkDb::request_execution_with_live_check`] (the same
//!    path `bossctl work start` uses) to mark the stale execution
//!    `abandoned` and insert a fresh `ready` execution, then kicks
//!    the coordinator's scheduler.
//! 8. Emits an [`Stage::OrphanActiveRedispatch`] dispatch event so
//!    the redispatch is visible in `bossctl dispatch tail`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_event_bus::{Event, EventBus, EventKind, TopicFilter};
use boss_protocol::{ExecutionKind, ExecutionStatus, RequestExecutionInput};

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::{
    ChurnTrip, ORPHAN_REDISPATCH_CHURN_GUARD_CONSECUTIVE_THRESHOLD, ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
    ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS, WorkDb,
};
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
    /// Items skipped because global dispatch is paused. The sweep mints
    /// executions through `WorkDb::request_execution_with_live_check`
    /// directly, so nothing downstream of it re-asks the pause question on
    /// its behalf: without this gate a pause made the sweep *more* active,
    /// not less, because a pause stops anything consuming worker slots and
    /// `has_idle_worker()` is then always true.
    pub dispatch_paused_skipped: usize,
    /// Items skipped because the row's most recent run ended in a
    /// deliberate engine park — a `boss propose done --outcome blocked`
    /// declaration, or the auto-nudge breaker giving up — whose attention
    /// item is still open. Those runs end `abandoned`, which this sweep
    /// used to read as "orphaned, redispatch".
    pub deliberate_park_skipped: usize,
    /// Items skipped because the pass could not *establish* whether it was
    /// allowed to redispatch — the admission evaluation or the `autostart`
    /// read failed. Counted separately from the gates themselves because
    /// this is an error signal, not a policy outcome: a non-zero count
    /// means rows are being held for a reason nobody chose.
    pub admission_unknown_skipped: usize,
}

impl crate::sweep_loop::SweepOutcome for OrphanSweepOutcome {
    fn has_activity(&self) -> bool {
        self.redispatched > 0
            || self.churn_skipped > 0
            || self.live_execution_skipped > 0
            || self.running_reviewer_skipped > 0
            || self.live_process_skipped > 0
            || self.dispatch_paused_skipped > 0
            || self.deliberate_park_skipped > 0
            || self.admission_unknown_skipped > 0
    }

    fn log(&self) {
        tracing::info!(
            redispatched = self.redispatched,
            churn_skipped = self.churn_skipped,
            no_worker_skipped = self.no_worker_skipped,
            live_execution_skipped = self.live_execution_skipped,
            running_reviewer_skipped = self.running_reviewer_skipped,
            live_process_skipped = self.live_process_skipped,
            dispatch_paused_skipped = self.dispatch_paused_skipped,
            deliberate_park_skipped = self.deliberate_park_skipped,
            admission_unknown_skipped = self.admission_unknown_skipped,
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

/// Subscribe to [`Event::ExecutionTerminal`] and redispatch the
/// terminated execution's work item immediately if it is now orphaned
/// (`active`, no live execution). This is the event-driven fast path
/// alongside [`spawn_loop`]'s 60s backstop — not a replacement for it:
/// delivery on the bus is best-effort (see `boss_event_bus::EventBus`),
/// so the periodic sweep still catches whatever a dropped or missed
/// event leaves behind.
///
/// Follows the subscriber crash-recovery contract documented on
/// [`boss_event_bus::spawn_supervised`]: every attempt — the first, and
/// each restart after a panic — runs a full [`run_one_pass`] before
/// settling into its event loop, so a subscriber crash never leaves an
/// orphan stranded until the next scheduled sweep tick.
pub fn spawn_event_subscriber(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    convergence: Arc<dyn LiveWorkerConvergence>,
    event_bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()> {
    boss_event_bus::spawn_supervised("orphan_sweep_event", move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        let convergence = Arc::clone(&convergence);
        let event_bus = Arc::clone(&event_bus);
        async move {
            run_one_pass(
                work_db.as_ref(),
                Arc::clone(&coordinator),
                dispatch_events.as_ref(),
                convergence.as_ref(),
            )
            .await;

            let mut subscription = event_bus.subscribe(TopicFilter::kind(EventKind::ExecutionTerminal));
            while let Some(event) = subscription.recv().await {
                let Event::ExecutionTerminal { task_id, .. } = event else {
                    continue;
                };
                run_one_pass_for_item(
                    work_db.as_ref(),
                    Arc::clone(&coordinator),
                    dispatch_events.as_ref(),
                    convergence.as_ref(),
                    &task_id,
                )
                .await;
            }
        }
    })
}

/// Run a single orphan-active sweep pass over every candidate in the
/// database. Returns a summary of what happened; callers may log it.
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
    run_one_pass_filtered(
        work_db,
        coordinator,
        dispatch_events,
        convergence,
        None,
        ORPHAN_MIN_AGE_SECS,
    )
    .await
}

/// Event-driven counterpart of [`run_one_pass`]: re-evaluates only
/// `work_item_id` instead of scanning every candidate in the database.
/// Called from [`spawn_event_subscriber`] when an
/// [`Event::ExecutionTerminal`] names this work item, so a post-crash
/// orphan is usually redispatched within milliseconds of the terminal
/// transition rather than waiting out the periodic sweep's interval.
///
/// Re-reads `work_item_id`'s candidacy from the DB rather than trusting
/// the event payload — events are hints, not commands (see the
/// event-bus design doc), so this is exactly as safe as a periodic pass
/// that happens to observe the same row. Idempotent with the periodic
/// sweep and with itself: a `work_item_id` that has already been
/// redispatched (or is no longer a candidate) simply is not present in
/// [`WorkDb::list_orphan_active_candidates`]'s result and this is a
/// no-op.
pub async fn run_one_pass_for_item(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    convergence: &dyn LiveWorkerConvergence,
    work_item_id: &str,
) -> OrphanSweepOutcome {
    run_one_pass_for_item_with_min_age(
        work_db,
        coordinator,
        dispatch_events,
        convergence,
        work_item_id,
        ORPHAN_MIN_AGE_SECS,
    )
    .await
}

/// Event-driven orphan-active pass for one work item with an explicit
/// candidate-age threshold. Kept alongside [`run_one_pass_for_item`] so a
/// real recovery fixture can exercise redispatch without waiting through the
/// production anti-race window.
pub async fn run_one_pass_for_item_with_min_age(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    convergence: &dyn LiveWorkerConvergence,
    work_item_id: &str,
    min_age_secs: i64,
) -> OrphanSweepOutcome {
    run_one_pass_filtered(
        work_db,
        coordinator,
        dispatch_events,
        convergence,
        Some(work_item_id),
        min_age_secs,
    )
    .await
}

/// Shared implementation behind [`run_one_pass`] and
/// [`run_one_pass_for_item`]. `only_work_item_id` restricts the pass to
/// a single candidate when set; `None` scans every candidate (the
/// periodic-sweep behavior).
async fn run_one_pass_filtered(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    convergence: &dyn LiveWorkerConvergence,
    only_work_item_id: Option<&str>,
    min_age_secs: i64,
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

    let candidates = match work_db.list_orphan_active_candidates(min_age_secs) {
        Ok(ids) => ids,
        Err(err) => {
            tracing::warn!(?err, "orphan sweep: failed to list candidates; skipping pass");
            return outcome;
        }
    };
    let candidates = match only_work_item_id {
        Some(id) => candidates.into_iter().filter(|c| c == id).collect(),
        None => candidates,
    };

    let now_epoch_secs: i64 = boss_engine_utils::epoch_time::now_epoch_secs();
    let churn_cutoff = now_epoch_secs - ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS;

    for work_item_id in candidates {
        // ── Admission gate 1: a deliberate engine park ────────────────
        //
        // This sweep redispatches any `active` row whose latest execution
        // is terminal, and it must not: `abandoned` is a terminal status,
        // and the engine writes it for *decisions* as well as for deaths.
        // `completion::run_done_declaration::finalize_declared_blocked`
        // (the worker declared `boss propose done --outcome blocked`) and
        // `completion::nudge`'s auto-nudge breaker both terminalize a run
        // as `abandoned` on purpose, release its slot and lease, and file
        // an attention item as the "a human should look at this" surface.
        // Redispatching such a row puts a replacement worker on exactly
        // the work a human was asked to adjudicate, every 60 seconds,
        // forever.
        //
        // Consulted through `WorkDb::dispatch_admission_facts` — the
        // engine's one reason-producing admission evaluator — rather than
        // a private copy of the attention query. The fact keys on
        // `work_executions.run_done_outcome = 'blocked'` (the durable
        // signal; the column is not cleared by `ClearedBy::WorkResumed`)
        // and on an open park attention item (the nudge-breaker park
        // never stamps the column).
        //
        // The discriminator is NOT the row's `autostart` flag.
        // `autostart` is single-shot: `start_execution_run` clears it the
        // first time a row enters `active` (`work/executions_runs.rs`, and
        // `migrate_backfill_autostart_consumed` backfilled the same for
        // older rows), so EVERY row this sweep can legitimately recover —
        // every row whose worker actually ran — has `autostart = 0`.
        // Gating this sweep on `autostart` would not honour the park; it
        // would switch the sweep off, post-crash orphan recovery included.
        //
        // Self-clearing for the attention half: both kinds are registered
        // `ClearedBy::WorkResumed` in `attention_lifecycle`, so an
        // operator's `bossctl work start` (or any fresh run) ends that
        // half with no separate gesture. The column half ends because the
        // new execution becomes latest and does not carry `blocked`. A
        // genuinely orphaned pane stamps neither, so recovery is
        // untouched.
        match work_db.dispatch_admission_facts(&work_item_id) {
            Ok(facts) if !facts.deliberate_parked => {}
            Ok(_) => {
                tracing::info!(
                    work_item_id = %work_item_id,
                    "orphan sweep: skipping redispatch — this row's run ended in a deliberate park \
                     (`bossctl work start` resumes it)",
                );
                dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::DispatchDecision, Outcome::Skipped, &work_item_id)
                            .with_work_item(&work_item_id)
                            .with_details(serde_json::json!({
                                "loop": "orphan_active_sweep",
                                "skipped_reason": "deliberate_park",
                            })),
                    )
                    .await;
                outcome.deliberate_park_skipped += 1;
                continue;
            }
            Err(err) => {
                // Fail loud and hold: an unreadable park state is not a
                // licence to put a second worker on the row.
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "orphan sweep: skipping redispatch — could not read the row's park state; \
                     refusing to redispatch on an unknown admission state",
                );
                outcome.admission_unknown_skipped += 1;
                continue;
            }
        }

        // ── Admission gate 2: the global dispatch pause ────────────────
        //
        // Asked through `ExecutionCoordinator::evaluate_dispatch_admission`,
        // the engine's one reason-producing admission evaluator, rather than
        // a second private notion of "is dispatch paused" — so this sweep
        // agrees by construction with what `bossctl dispatch pause` means
        // everywhere else, including the one case where a paused engine
        // legitimately still dispatches: an operator-originated pause
        // exempts the review pool (`drain_ready_queue` holds only
        // `paused && !is_review`), and the evaluator reports no pause in
        // effect for such a row. That exemption is the *only* sanctioned
        // bypass here, and it is the evaluator's decision, not this sweep's.
        //
        // Deliberately reads only `admission.pause`, not `would_dispatch`:
        // the other blockers it computes (the interactive concurrency cap
        // above all) govern how many workers run at once, which is the
        // `has_idle_worker` question this sweep already asks its own way.
        // Widening the gate to every blocker would change what orphan
        // recovery waits on, and orphan recovery must keep firing.
        //
        // Note what this does NOT do: it does not pause or suspend the
        // sweep. The sweep keeps running, keeps evaluating, and keeps
        // logging; it just does not mint an execution while dispatch is
        // paused. Orphan recovery resumes on the first pass after the
        // pause lifts.
        let admission = match coordinator.evaluate_dispatch_admission(&work_item_id).await {
            Ok(admission) => admission,
            Err(err) => {
                // Fail loud and hold. An admission evaluation that cannot
                // be computed is not a licence to redispatch — that is
                // exactly how the row would get revived through a pause.
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "orphan sweep: skipping redispatch — could not evaluate dispatch admission; \
                     refusing to redispatch on an unknown pause state",
                );
                outcome.admission_unknown_skipped += 1;
                continue;
            }
        };
        if admission.pause.active {
            tracing::info!(
                work_item_id = %work_item_id,
                pause_origin = admission.pause.origin.as_deref().unwrap_or("unknown"),
                pause_reason = admission.pause.reason.as_deref().unwrap_or("no reason recorded"),
                "orphan sweep: skipping redispatch — global dispatch is paused",
            );
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::DispatchHeldByPause, Outcome::Skipped, &work_item_id)
                        .with_work_item(&work_item_id)
                        .with_details(serde_json::json!({
                            "loop": "orphan_active_sweep",
                            "admission": "orphan_sweep_redispatch",
                            "origin": admission.pause.origin,
                            "reason": admission.pause.reason,
                            "paused_since_epoch_s": admission.pause.paused_since_epoch_s,
                            "overridable": admission.pause.overridable,
                        })),
                )
                .await;
            outcome.dispatch_paused_skipped += 1;
            continue;
        }

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
        // The trailing window alone cannot catch a loop slower than
        // `WINDOW_SECS / THRESHOLD`: each cycle's oldest evidence ages out
        // before the next failure lands inside the window, so the count
        // plateaus below the threshold no matter how many workers the row
        // burns. Observed 2026-09-13 at a 40-50 minute cycle time: at one
        // row's redispatch only two of its terminal executions fell inside
        // the one-hour cutoff, one short of tripping, forever. Lengthening
        // the window does not fix that — a slower loop outruns any fixed
        // window — so the guard has a second, time-independent half: the
        // unbroken streak of unproductive terminal executions since the
        // row last completed a run. Either half tripping parks the row.
        let consecutive_terminal_ids = match work_db.list_consecutive_unproductive_terminal_execution_ids(&work_item_id)
        {
            Ok(ids) => ids,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "orphan sweep: failed to count consecutive terminal executions; skipping item",
                );
                continue;
            }
        };
        let consecutive_terminal = consecutive_terminal_ids.len() as i64;
        let window_tripped = recent_terminal >= ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD;
        let consecutive_tripped = consecutive_terminal >= ORPHAN_REDISPATCH_CHURN_GUARD_CONSECUTIVE_THRESHOLD;
        let churn_tripped = window_tripped || consecutive_tripped;

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
                            "predicate": format!(
                                "tasks.status='active' AND no ready execution AND updated_at age >= {min_age_secs}s"
                            ),
                            "live_execution_id": live.id,
                            "live_execution_status": live.status,
                            "live_execution_claimed": live_claimed,
                            "recent_terminal_executions": recent_terminal,
                            "consecutive_terminal_executions": consecutive_terminal,
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
            // The windowed half stays the reported basis when both trip:
            // it is the tighter statement (this many failures *and* this
            // fast), and it is the one that clears on its own.
            let trip = if window_tripped {
                ChurnTrip::Window
            } else {
                ChurnTrip::Consecutive
            };
            tracing::warn!(
                work_item_id = %work_item_id,
                recent_terminal,
                consecutive_terminal,
                window_tripped,
                consecutive_tripped,
                threshold = ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
                window_secs = ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS,
                consecutive_threshold = ORPHAN_REDISPATCH_CHURN_GUARD_CONSECUTIVE_THRESHOLD,
                "orphan sweep: churn guard tripped; skipping redispatch — human attention required",
            );
            let (counted, failing_ids) = match trip {
                ChurnTrip::Window => (
                    recent_terminal,
                    work_db
                        .list_recent_terminal_execution_ids(&work_item_id, churn_cutoff, None)
                        .unwrap_or_default(),
                ),
                ChurnTrip::Consecutive => (consecutive_terminal, consecutive_terminal_ids),
            };
            work_db.bounce_churn_guard_parked_to_backlog(
                &work_item_id,
                "orphan_sweep",
                counted,
                &failing_ids,
                "terminal executions",
                trip,
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
                        "consecutive_terminal_executions": consecutive_terminal,
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
    use crate::work::{CreateRevisionInput, ExecutionStatus, PrOpenState, StaticPrStateChecker, WorkDb, WorkItemPatch};
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

    // ─── admission gates (pause / autostart) ────────────────────────────

    /// **The 2026-09-13 paused-dispatch redispatch.** Global dispatch was
    /// paused, and this sweep minted a fresh execution anyway — abandoning
    /// the predecessor's work in the process. Its only admission gate was
    /// `has_idle_worker()`, which is a slot-occupancy question, not an
    /// admission question; worse, a pause stops anything *consuming* worker
    /// slots, so pausing dispatch made this sweep strictly more likely to
    /// fire, not less.
    #[tokio::test]
    async fn does_not_redispatch_while_dispatch_is_paused() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let now = boss_engine_utils::epoch_time::now_epoch_secs().max(0) as u64;
        coordinator.pause_dispatch(
            now,
            crate::coordinator::DispatchPauseOrigin::Operator,
            boss_protocol::PauseReason::new("test: operator paused dispatch").unwrap(),
        );

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
            "a paused dispatcher must not have work redispatched onto it behind its back",
        );
        assert_eq!(outcome.dispatch_paused_skipped, 1);

        // The destructive half is creating the row at all: doing so marks
        // the predecessor `abandoned` and discards its workspace.
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert_eq!(
            executions.len(),
            1,
            "no fresh execution may be minted while dispatch is paused; got {executions:?}",
        );

        let events = sink.events().await;
        let held: Vec<_> = events.iter().filter(|e| e.stage == "dispatch_held_by_pause").collect();
        assert_eq!(held.len(), 1, "the hold must be visible in the dispatch stream");
        assert_eq!(held[0].outcome, "skipped");
        assert_eq!(
            held[0].details["admission"],
            serde_json::json!("orphan_sweep_redispatch"),
        );
        assert!(
            events.iter().all(|e| e.stage != "orphan_active_redispatch"),
            "no redispatch event may fire while dispatch is paused",
        );
    }

    /// The pause gate holds the redispatch; it does not disable the sweep.
    /// The same row recovers on the first pass after the pause lifts —
    /// orphan recovery is deferred by a pause, never cancelled by one.
    #[tokio::test]
    async fn redispatches_once_the_pause_lifts() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let now = boss_engine_utils::epoch_time::now_epoch_secs().max(0) as u64;
        coordinator.pause_dispatch(
            now,
            crate::coordinator::DispatchPauseOrigin::Operator,
            boss_protocol::PauseReason::new("test: operator paused dispatch").unwrap(),
        );
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let held = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;
        assert_eq!(held.redispatched, 0, "precondition: the pause held it");

        coordinator.resume_dispatch();
        let resumed = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(
            resumed.redispatched, 1,
            "a lifted pause must let the genuinely-orphaned row recover — the sweep is deferred \
             by a pause, not disabled by one",
        );
        assert_eq!(resumed.dispatch_paused_skipped, 0);
    }

    /// A breaker-origin pause holds the redispatch exactly as an operator
    /// one does. The sweep asks the shared admission evaluator rather than
    /// carrying its own idea of what a pause means, so it inherits every
    /// pause's real scope instead of re-deciding it.
    #[tokio::test]
    async fn a_breaker_pause_also_holds_the_redispatch() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let now = boss_engine_utils::epoch_time::now_epoch_secs().max(0) as u64;
        coordinator.pause_dispatch(
            now,
            crate::coordinator::DispatchPauseOrigin::Breaker,
            boss_protocol::PauseReason::new("test: breaker tripped").unwrap(),
        );

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;

        assert_eq!(outcome.redispatched, 0);
        assert_eq!(outcome.dispatch_paused_skipped, 1);
        let events = sink.events().await;
        let held: Vec<_> = events.iter().filter(|e| e.stage == "dispatch_held_by_pause").collect();
        assert_eq!(held[0].details["origin"], serde_json::json!("breaker"));
        assert_eq!(held[0].details["overridable"], serde_json::json!(false));
    }

    /// **The defeated park.** `finalize_declared_blocked` ends a run the
    /// worker declared itself blocked on: the execution goes `abandoned`,
    /// its slot and lease are released, and an open attention item asks a
    /// human to adjudicate. `abandoned` is terminal, so this sweep read the
    /// row as a legitimate orphan and put a replacement worker on exactly
    /// the work that was handed to the human — every 60 seconds, forever.
    ///
    /// Note what the discriminator is NOT: `autostart`. That flag is
    /// single-shot and `start_execution_run` consumes it the first time a
    /// row goes `active`, so every row this sweep can legitimately recover
    /// already has `autostart = 0` — see
    /// `does_not_gate_recovery_on_the_single_shot_autostart_flag`.
    #[tokio::test]
    async fn does_not_revive_a_deliberately_parked_row() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.record_worker_idle_abandonment(&execution_id, "worker declared itself blocked")
            .unwrap();
        db.create_attention_item(boss_protocol::CreateAttentionItemInput {
            execution_id: Some(execution_id.clone()),
            work_item_id: None,
            kind: crate::completion::RUN_DONE_BLOCKED_ATTENTION_KIND.to_owned(),
            status: None,
            title: "Run ended: worker declared itself blocked".to_owned(),
            body_markdown: "blocked".to_owned(),
            resolved_at: None,
        })
        .unwrap();
        // Age LAST: the writes above touch `tasks.updated_at`.
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

        assert_eq!(
            outcome.redispatched, 0,
            "a row whose run was deliberately parked must not get a replacement worker",
        );
        assert_eq!(outcome.deliberate_park_skipped, 1);
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert_eq!(executions.len(), 1, "no replacement execution may be minted");
        let events = sink.events().await;
        assert!(
            events.iter().all(|e| e.stage != "orphan_active_redispatch"),
            "no redispatch event may fire for a parked row",
        );
        let skipped: Vec<_> = events
            .iter()
            .filter(|e| e.stage == "dispatch_decision" && e.details["skipped_reason"] == "deliberate_park")
            .collect();
        assert_eq!(skipped.len(), 1, "the park must be visible in the dispatch stream");
    }

    /// The park is a park, not a tombstone. Only an OPEN park item holds
    /// the row: once it is resolved — by a human reviewing it, or
    /// automatically by `ClearedBy::WorkResumed` when a fresh run starts —
    /// the same row is recovered normally.
    #[tokio::test]
    async fn a_resolved_park_attention_does_not_hold_the_row() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.record_worker_idle_abandonment(&execution_id, "nudge breaker parked the run")
            .unwrap();
        db.create_attention_item(boss_protocol::CreateAttentionItemInput {
            execution_id: Some(execution_id.clone()),
            work_item_id: None,
            kind: crate::completion::NUDGE_BREAKER_ATTENTION_KIND.to_owned(),
            status: Some("resolved".to_owned()),
            title: "Worker parked: auto-nudge loop bounded".to_owned(),
            body_markdown: "parked".to_owned(),
            resolved_at: Some(boss_engine_utils::iso8601::format_epoch_iso8601(
                boss_engine_utils::epoch_time::now_epoch_secs(),
            )),
        })
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

        assert_eq!(
            outcome.redispatched, 1,
            "a settled park must not keep the row out of the sweep's hands forever",
        );
        assert_eq!(outcome.deliberate_park_skipped, 0);
    }

    /// **The gate that would have switched the sweep off.** `autostart` is
    /// single-shot — `start_execution_run` clears it the first time a row
    /// enters `active` — so the flag reads `false` on *every* row this
    /// sweep exists to recover, a genuinely orphaned pane included. This
    /// test pins that: a row whose worker really died, with `autostart`
    /// consumed exactly as production leaves it, must still recover.
    #[tokio::test]
    async fn does_not_gate_recovery_on_the_single_shot_autostart_flag() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());
        db.mark_execution_orphaned(&execution_id, "worker died").unwrap();
        make_old(&db, &work_item_id);

        assert!(
            !get_task(&db, &work_item_id).autostart,
            "precondition: a row that has run carries autostart = false — if this ever changes, \
             the reasoning behind the park gate needs revisiting",
        );

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

        assert_eq!(
            outcome.redispatched, 1,
            "post-crash orphan recovery is the reason this sweep exists; a consumed autostart \
             flag must never stop it",
        );
    }

    /// **The churn guard a slow loop outruns.** Three unproductive terminal
    /// executions 45 minutes apart: at every redispatch only two of them are
    /// inside the one-hour trailing window, so the windowed count plateaus
    /// one short of the threshold forever, however many workers the row
    /// burns. The time-independent half must trip on the same evidence.
    #[tokio::test]
    async fn churn_guard_trips_on_a_slow_loop_the_window_cannot_catch() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        let cycle_secs = 45 * 60;
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_CONSECUTIVE_THRESHOLD {
            db.insert_terminal_execution_for_test(
                &work_item_id,
                "chore_implementation",
                "abandoned",
                now_epoch - i * cycle_secs,
            )
            .unwrap();
        }

        // Precondition: the windowed half genuinely cannot see this. If this
        // assertion ever fails the test has stopped exercising a slow loop.
        let windowed = db
            .count_recent_terminal_executions(
                &work_item_id,
                now_epoch - ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS,
                None,
            )
            .unwrap();
        assert!(
            windowed < ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
            "precondition: the trailing window must NOT be able to trip here (saw {windowed})",
        );

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

        assert_eq!(
            outcome.churn_skipped, 1,
            "an unbroken streak of dead runs must park the row however long it took",
        );
        assert_eq!(outcome.redispatched, 0);

        let task = get_task(&db, &work_item_id);
        assert_eq!(task.dispatch_failed_reason.as_deref(), Some("churn_guard"));
        assert!(
            task.dispatch_failed_error
                .as_deref()
                .is_some_and(|e| e.contains("consecutive")),
            "the park text must name the half that actually tripped: {:?}",
            task.dispatch_failed_error,
        );
    }

    /// The consecutive half counts a *streak*, not a lifetime total: a run
    /// that completed resets it. Without this the guard would park any
    /// long-lived row that had accumulated enough failures across its whole
    /// history, which is not churn.
    #[tokio::test]
    async fn a_completed_run_resets_the_consecutive_churn_count() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        let cycle_secs = 45 * 60;
        // Old failures, then a success, then one fresh failure: the streak
        // is 1, even though the row's lifetime failure count is over the
        // threshold.
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_CONSECUTIVE_THRESHOLD {
            db.insert_terminal_execution_for_test(
                &work_item_id,
                "chore_implementation",
                "abandoned",
                now_epoch - (i + 2) * cycle_secs,
            )
            .unwrap();
        }
        db.insert_terminal_execution_for_test(
            &work_item_id,
            "chore_implementation",
            "completed",
            now_epoch - cycle_secs,
        )
        .unwrap();
        db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "abandoned", now_epoch)
            .unwrap();

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

        assert_eq!(
            outcome.churn_skipped, 0,
            "a row that delivered since its failures is not churning",
        );
        assert_eq!(outcome.redispatched, 1);
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

    // ── pending-review hold vs. genuine orphan ──────────────────────────────

    /// Insert a minimal `pr_review_batches` row directly. Test-only: the
    /// production path (`WorkDb::create_pre_merge_review_batch_for_pool`)
    /// requires a `gh pr view` round trip and pool-admission bookkeeping
    /// this suite has no need to exercise — only the row shape
    /// `list_orphan_active_candidates`'s exclusion reads matters here.
    /// Does not touch `tasks.pr_head_sha`: the hold path never writes that
    /// column, so tests must not stamp it either.
    fn insert_review_batch(db: &WorkDb, cycle_root_id: &str, status: &str, target_sha: &str, pr_url: &str) {
        let conn = db.connect().unwrap();
        let now = boss_engine_utils::epoch_time::now_epoch_secs().to_string();
        conn.execute(
            "INSERT INTO pr_review_batches (
                 id, cycle_root_id, base_sha, classification_json, created_at,
                 phase, pr_number, pr_url, status, target_sha, updated_at
             ) VALUES (?1, ?2, 'base-sha', '{}', ?3, 'pre_merge', 1, ?4, ?5, ?6, ?3)",
            rusqlite::params![
                format!("batch-{cycle_root_id}-{status}-{target_sha}"),
                cycle_root_id,
                now,
                pr_url,
                status,
                target_sha
            ],
        )
        .unwrap();
    }

    /// The ReviewerEnqueued hold shape: a completed producing execution on
    /// an `active` task, with no live execution left. Age last — execution
    /// writes bump `tasks.updated_at`. The `connect()` guard is dropped
    /// before `make_old`, which also connects; holding both deadlocks the
    /// single-connection pool.
    fn hold_with_completed_producer(db: &WorkDb, work_item_id: &str) {
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.to_owned())
                    .build(),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = 'completed', finished_at = '1' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();
        }
        make_old(db, work_item_id);
    }

    /// Stamp `pr_head_after` on the work item's latest execution. Production
    /// writes this from `record_worker_pr_completion`; the hold helper above
    /// uses a direct status update so tests that care about freshness must
    /// set it themselves.
    fn stamp_latest_pr_head_after(db: &WorkDb, work_item_id: &str, sha: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET pr_head_after = ?1
             WHERE id = (
                 SELECT id FROM work_executions
                 WHERE work_item_id = ?2
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1
             )",
            rusqlite::params![sha, work_item_id],
        )
        .unwrap();
    }

    /// First-PR chore: the chore is its own cycle root, `pr_head_sha` is
    /// NULL (the hold path never writes it), and a live pre_merge batch
    /// exists. This is the common production hold; a sha-keyed exclusion
    /// against `tasks.pr_head_sha` would miss it. An unknown
    /// `pr_head_after` still excludes.
    #[tokio::test]
    async fn first_pr_chore_with_live_pre_merge_batch_and_null_pr_head_sha_is_not_an_orphan_candidate() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        hold_with_completed_producer(&db, &work_item_id);

        insert_review_batch(&db, &work_item_id, "supervising", "sha-current", "https://example/pr/1");

        assert!(
            !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
                .unwrap()
                .contains(&work_item_id),
            "a first-PR chore held pending a live pre_merge batch must not be an orphan candidate, \
             even with tasks.pr_head_sha still NULL"
        );
    }

    /// Acceptance: the same task IS a candidate again once the review batch
    /// reaches a terminal state — the hold must not become permanent immunity.
    #[tokio::test]
    async fn held_task_becomes_orphan_candidate_once_review_batch_terminates() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        hold_with_completed_producer(&db, &work_item_id);

        insert_review_batch(&db, &work_item_id, "completed", "sha-current", "https://example/pr/1");

        assert!(
            db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
                .unwrap()
                .contains(&work_item_id),
            "a task whose review batch has already terminated must become a candidate again"
        );
    }

    /// Acceptance: a genuinely orphaned `active` task — no live execution, no
    /// open review batch at all — is still returned, so orphan recovery for
    /// the ordinary case is not weakened by this exclusion.
    #[tokio::test]
    async fn genuinely_orphaned_task_with_no_review_batch_is_still_a_candidate() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        assert!(
            db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
                .unwrap()
                .contains(&work_item_id),
            "a task with no live execution and no review batch at all must remain an orphan candidate"
        );
    }

    /// Acceptance: a held task whose latest producer `pr_head_after` has
    /// moved on from the live batch's `target_sha` is an orphan candidate
    /// again — a stale batch must not grant immunity until the reaper
    /// fires. Uses `pr_head_after` (written at hold time), not
    /// `tasks.pr_head_sha`.
    #[tokio::test]
    async fn held_task_whose_producer_head_moved_past_batch_target_is_an_orphan_candidate() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        hold_with_completed_producer(&db, &work_item_id);
        stamp_latest_pr_head_after(&db, &work_item_id, "sha-new");

        insert_review_batch(&db, &work_item_id, "supervising", "sha-old", "https://example/pr/1");

        assert!(
            db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
                .unwrap()
                .contains(&work_item_id),
            "a live pre_merge batch whose target_sha is behind the producer head must not mask the task"
        );
    }

    /// Matching `pr_head_after` and `target_sha` is the live-hold shape
    /// once GitHub head was captured at completion: still excluded.
    #[tokio::test]
    async fn live_pre_merge_batch_with_matching_producer_head_is_not_an_orphan_candidate() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        hold_with_completed_producer(&db, &work_item_id);
        stamp_latest_pr_head_after(&db, &work_item_id, "sha-current");

        insert_review_batch(&db, &work_item_id, "supervising", "sha-current", "https://example/pr/1");

        assert!(
            !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
                .unwrap()
                .contains(&work_item_id),
            "a live pre_merge batch still targeting the producer head must exclude the task"
        );
    }

    /// ReviewerEnqueued on a revision: the batch is keyed on the PR-owning
    /// ancestor (the cycle root), not the revision's own id. The recursive
    /// walk's UNION ALL branch has to fire for the exclusion to see it.
    /// `pr_head_sha` stays NULL on both rows — production never stamps it
    /// at hold time.
    #[tokio::test]
    async fn held_revision_with_batch_keyed_on_cycle_root_is_not_an_orphan_candidate() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let root_id = create_active_chore(&db, &product_id, "pr-owning chore");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET pr_url = ?1, status = 'in_review' WHERE id = ?2",
                rusqlite::params!["https://example/pr/1", root_id],
            )
            .unwrap();
        }
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(root_id.clone())
                    .description("address review findings")
                    .autostart(false)
                    .build(),
                &StaticPrStateChecker(PrOpenState::Open),
            )
            .unwrap();
        db.update_work_item(
            &revision.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        hold_with_completed_producer(&db, &revision.id);
        insert_review_batch(&db, &root_id, "supervising", "sha-current", "https://example/pr/1");

        assert!(
            !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
                .unwrap()
                .contains(&revision.id),
            "a held revision must be excluded via the cycle-root walk, with the batch keyed on the \
             PR-owning ancestor and pr_head_sha left NULL"
        );
    }

    /// Two completed producers on the same held task: the older one stamped
    /// `pr_head_after='sha-A'`, the latest left unstamped (the fail-open
    /// `fetch_pr_head_after` outcome). A live batch targeting `sha-B` must
    /// still exclude — unknown latest head is not replaced by the stale SHA.
    #[tokio::test]
    async fn held_task_latest_unstamped_producer_excludes_even_when_older_producer_has_a_different_sha() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        hold_with_completed_producer(&db, &work_item_id);
        stamp_latest_pr_head_after(&db, &work_item_id, "sha-A");

        let later = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = 'completed', finished_at = '2' WHERE id = ?1",
                rusqlite::params![later.id],
            )
            .unwrap();
        }
        make_old(&db, &work_item_id);

        insert_review_batch(&db, &work_item_id, "supervising", "sha-B", "https://example/pr/1");

        assert!(
            !db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS)
                .unwrap()
                .contains(&work_item_id),
            "the latest producer's unknown pr_head_after must still exclude, even when an older \
             producer recorded a SHA that does not match the live batch target"
        );
    }

    /// Two active tasks can share one cycle root (chain root + revision). A
    /// live pre_merge batch must exclude only the held producer, not a
    /// sibling that has never completed a producer execution — that sibling
    /// is a genuine orphan and the COALESCE-to-target fallback must not
    /// shield it.
    #[tokio::test]
    async fn live_batch_does_not_exclude_same_cycle_root_sibling_with_no_producer() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let root_id = create_active_chore(&db, &product_id, "pr-owning chore");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
                rusqlite::params!["https://example/pr/1", root_id],
            )
            .unwrap();
        }
        hold_with_completed_producer(&db, &root_id);
        insert_review_batch(&db, &root_id, "supervising", "sha-current", "https://example/pr/1");

        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(root_id.clone())
                    .description("address review findings")
                    .autostart(false)
                    .build(),
                &StaticPrStateChecker(PrOpenState::Open),
            )
            .unwrap();
        db.update_work_item(
            &revision.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        make_old(&db, &revision.id);

        let candidates = db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS).unwrap();
        assert!(
            !candidates.contains(&root_id),
            "the held producer under a live pre_merge batch must still be excluded"
        );
        assert!(
            candidates.contains(&revision.id),
            "an active sibling with no producer completion must remain an orphan candidate, even \
             while a live pre_merge batch is open on the shared cycle root"
        );
    }

    // ── event-driven path (run_one_pass_for_item / spawn_event_subscriber) ──

    /// `run_one_pass_for_item` redispatches the named orphan, same as a full
    /// `run_one_pass` would, when it is a genuine candidate.
    #[tokio::test]
    async fn run_one_pass_for_item_redispatches_matching_orphan() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass_for_item(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
            &work_item_id,
        )
        .await;

        assert_eq!(outcome.redispatched, 1);
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions.iter().any(|e| e.status == ExecutionStatus::Ready),
            "expected a ready execution after the event-driven redispatch"
        );
    }

    /// `run_one_pass_for_item` never acts on a work item other than the one
    /// named — an `ExecutionTerminal` event for a different task must not
    /// cause an unrelated orphan to be touched.
    #[tokio::test]
    async fn run_one_pass_for_item_ignores_other_work_items() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass_for_item(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
            "task_unrelated",
        )
        .await;

        assert_eq!(outcome.redispatched, 0);
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions.is_empty(),
            "the named-but-unrelated work item must be left untouched"
        );
    }

    /// Idempotency: once the periodic sweep (`run_one_pass`) has already
    /// redispatched an orphan, a subsequent event-driven pass
    /// (`run_one_pass_for_item`) for the same work item — e.g. the
    /// `ExecutionTerminal` event racing the sweep that already reconciled
    /// it — must be a no-op rather than double-dispatching.
    #[tokio::test]
    async fn event_driven_pass_is_idempotent_with_periodic_sweep() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let first = run_one_pass(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
        )
        .await;
        assert_eq!(
            first.redispatched, 1,
            "periodic sweep should redispatch the orphan once"
        );

        let second = run_one_pass_for_item(
            db.as_ref(),
            coordinator.clone(),
            sink.as_ref(),
            &NoopLiveWorkerConvergence,
            &work_item_id,
        )
        .await;
        assert_eq!(
            second.redispatched, 0,
            "event-driven pass for the same work item must be a no-op once already redispatched"
        );

        let ready_count = db
            .list_executions(Some(&work_item_id))
            .unwrap()
            .into_iter()
            .filter(|e| e.status == ExecutionStatus::Ready)
            .count();
        assert_eq!(ready_count, 1, "exactly one ready execution, not a duplicate");
    }

    /// End-to-end: `spawn_event_subscriber` actually redispatches an
    /// orphan when an `ExecutionTerminal` event is published on the bus.
    /// The subscriber's initial full reconcile pass is deliberately
    /// starved of an idle worker slot (the pool's one slot is pre-claimed)
    /// so it observes `no_worker_skipped` and does nothing — isolating the
    /// assertion to the event-driven path rather than the startup
    /// reconcile that every subscriber also runs.
    #[tokio::test]
    async fn spawn_event_subscriber_redispatches_on_execution_terminal() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let dummy_worker_id = coordinator
            .worker_pool()
            .claim_worker("dummy-exec-id", None)
            .await
            .expect("test pool must have a slot to claim");

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let bus = Arc::new(EventBus::new());

        let _handle = spawn_event_subscriber(
            db.clone(),
            coordinator.clone(),
            sink.clone(),
            Arc::new(NoopLiveWorkerConvergence),
            bus.clone(),
        );

        // Let the subscriber's initial full-reconcile pass run and observe
        // no idle worker before we free the slot below.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            db.list_executions(Some(&work_item_id)).unwrap().is_empty(),
            "startup reconcile must not have redispatched while the pool was fully claimed"
        );

        coordinator.worker_pool().release_worker(&dummy_worker_id, None).await;

        bus.publish(Event::ExecutionTerminal {
            execution_id: "dummy-exec-id".to_owned(),
            task_id: work_item_id.clone(),
            host_id: "local".to_owned(),
            pool_claim: None,
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let executions = db.list_executions(Some(&work_item_id)).unwrap();
                if executions.iter().any(|e| e.status == ExecutionStatus::Ready) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("event-driven redispatch did not happen before the timeout");
    }
}
