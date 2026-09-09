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
//! 1. Snapshots execution claims across all worker pools.
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
//! 3. Determines, per candidate, whether the row's last run ended in a
//!    deliberate engine park (a `boss propose done --outcome blocked`
//!    declaration, or the auto-nudge breaker giving up — both terminalize
//!    the run as `abandoned` and leave an open attention item), and
//!    evaluates the churn guard (step 6) alongside it. Both conditions must
//!    reach the halted-state bounce after the liveness guards, independently
//!    of dispatch pause or worker capacity. An unreadable park state fails
//!    closed rather than defaulting to redispatch.
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
//!    the deliberate-park and churn-guard state it evaluated earlier in the
//!    pass (step 3). The churn guard has two halves and either one trips it:
//!    [`ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD`] terminal executions inside
//!    the trailing [`ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS`] (fast
//!    churn), or [`ORPHAN_REDISPATCH_CHURN_GUARD_CONSECUTIVE_THRESHOLD`]
//!    *consecutive* unproductive terminal executions with no successful run
//!    in between, however long they took (slow churn, which no trailing
//!    window can catch — see that constant's docs). A deliberate park or a
//!    churn trip (or both at once) is bounced to Backlog: a park-only or
//!    park-plus-churn row goes through
//!    [`crate::work::WorkDb::bounce_deliberate_park_to_backlog`]
//!    (`dispatch_failed_reason = "deliberate_park"`, with the churn detail
//!    folded into the body when both conditions hold — the park wins the
//!    reason because it is the stronger, human-only-clearable condition), and
//!    a churn-only row keeps going through
//!    [`crate::work::WorkDb::bounce_churn_guard_parked_to_backlog`]
//!    (`dispatch_failed_reason = "churn_guard"`). Both are
//!    the same `dispatch_failed_reason` surface a pre-spawn dispatch failure
//!    uses, so the kanban board shows the halt instead of the card sitting
//!    in Doing looking like ordinary work in progress — see
//!    `docs/designs/dispatch-halt-state-vs-attention-items.md`. The bounce is
//!    deliberately sequenced *after* steps 4 and 5: those are the only
//!    checks that can tell a churn-tripped row apart from a row whose
//!    previous worker process is still alive (a live-but-untracked worker
//!    tends to also produce the terminal-execution churn that trips this
//!    guard), and bouncing first would demote a row to Backlog with a
//!    halted-state banner while its previous worker is still editing the
//!    workspace. The churn-only path auto-clears once
//!    [`crate::dispatch_failure_recovery_sweep`] retries it after its
//!    cooldown: that sweep recognises a `CHURN_GUARD_DISPATCH_FAILED_REASON`
//!    row and applies *this* guard's own thresholds to it — both halves, not
//!    its own looser 5-in-24h one — so the contract carries over unchanged
//!    rather than being weakened by the representation change (its
//!    10-minute cooldown is shorter than any window, so without the
//!    consecutive half it would un-park a slow loop one cooldown at a
//!    time). A `deliberate_park` row is different: that sweep excludes it
//!    entirely, because a park is a human decision, not a condition that
//!    resolves on its own — only an explicit `bossctl work start` / kanban
//!    drag-to-Doing clears it, the same gesture that already resolves the
//!    park's own attention item.
//! 7. Checks dispatch pause and worker capacity, then calls [`WorkDb::request_execution_with_live_check`] (the same
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
    /// Items whose most recent run ended in a deliberate engine park — a
    /// `boss propose done --outcome blocked` declaration, or the auto-nudge
    /// breaker giving up — whose attention item is still open. Those runs
    /// end `abandoned`, but must never be redispatched. A subset of these
    /// also reach [`Self::deliberate_park_bounced`] once the liveness guards
    /// below have cleared them for the mutating halted-state bounce.
    pub deliberate_park_skipped: usize,
    /// Items actually bounced to Backlog by
    /// [`crate::work::WorkDb::bounce_deliberate_park_to_backlog`] because a
    /// deliberate park (counted above in `deliberate_park_skipped`) survived
    /// the live-execution and durable-process guards. This is the halted-
    /// state surface the kanban card reads, independent of the open
    /// attention item (`docs/designs/dispatch-halt-state-vs-attention-items.md`).
    /// Only incremented when the write actually landed (the bounce helper
    /// returns `true`), so a no-op (the row raced a status change) or a DB
    /// write failure never inflates this counter with a halt that was never
    /// made visible on the board.
    pub deliberate_park_bounced: usize,
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
            || self.deliberate_park_bounced > 0
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
            deliberate_park_bounced = self.deliberate_park_bounced,
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
        // Deliberately does NOT `continue` on a positive park read the way
        // this used to: that early exit is exactly what made a parked row
        // invisible — it skipped past the churn read below before that
        // code ever ran, so a row that was BOTH parked and churning never
        // reached the mutating bounce that would have surfaced either
        // condition. Instead this carries `is_deliberately_parked` forward
        // through the churn read, the pause gate, and the two liveness
        // guards, and only the combined decision after those guards
        // (below) decides whether to bounce. `Err` still fails closed
        // exactly as before: an unreadable park state is not a licence to
        // put a second worker on the row, and there is nothing useful to
        // combine it with.
        let is_deliberately_parked = match work_db.dispatch_admission_facts(&work_item_id) {
            Ok(facts) => facts.deliberate_parked,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "orphan sweep: skipping redispatch — could not read the row's park state; \
                     refusing to redispatch on an unknown admission state",
                );
                outcome.admission_unknown_skipped += 1;
                continue;
            }
        };

        if is_deliberately_parked {
            tracing::info!(
                work_item_id = %work_item_id,
                "orphan sweep: this row's run ended in a deliberate park with an open attention item; \
                 will not redispatch it, and will bounce it to the halted-state surface once the \
                 liveness guards below confirm no worker of its own is still running \
                 (`bossctl work start` resumes it either way)",
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

            // Record declined blocks with the probed pid, result, and hook
            // age so a redispatch decision can be diagnosed from its own tail.
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

        // Now apply the deliberate-park and/or churn-guard mutation. Both
        // liveness guards above have already run and neither skipped this
        // candidate, so we know (as well as this sweep ever can) that there
        // is no live execution and no live previous-worker process for this
        // row — only now is it safe to bounce it to Backlog with a halted-
        // state banner.
        if is_deliberately_parked || churn_tripped {
            // The windowed half stays the reported basis when both churn
            // halves trip: it is the tighter statement (this many failures
            // *and* this fast), and it is the one that clears on its own.
            let churn_trip_info = churn_tripped.then(|| {
                let trip = if window_tripped {
                    ChurnTrip::Window
                } else {
                    ChurnTrip::Consecutive
                };
                let (counted, failing_ids) = match trip {
                    ChurnTrip::Window => (
                        recent_terminal,
                        work_db
                            .list_recent_terminal_execution_ids(&work_item_id, churn_cutoff, None)
                            .unwrap_or_default(),
                    ),
                    ChurnTrip::Consecutive => (consecutive_terminal, consecutive_terminal_ids),
                };
                (trip, counted, failing_ids)
            });

            if is_deliberately_parked {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    churn_tripped,
                    recent_terminal,
                    consecutive_terminal,
                    "orphan sweep: deliberate park confirmed clear of both liveness guards; bouncing to \
                     Backlog so the halted state is visible on the kanban card instead of only the open \
                     attention item",
                );
                let bounced = work_db.bounce_deliberate_park_to_backlog(
                    &work_item_id,
                    "orphan_sweep",
                    churn_trip_info
                        .as_ref()
                        .map(|(trip, counted, ids)| (*trip, *counted, ids.as_slice())),
                );
                if bounced {
                    outcome.deliberate_park_bounced += 1;
                }
            } else if let Some((trip, counted, failing_ids)) = churn_trip_info {
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
                work_db.bounce_churn_guard_parked_to_backlog(
                    &work_item_id,
                    "orphan_sweep",
                    counted,
                    &failing_ids,
                    "terminal executions",
                    trip,
                );
                outcome.churn_skipped += 1;
            }
            continue;
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

        // Capacity governs redispatch, never the halted-state surface. This
        // check stays below the pause-admission evaluation above rather than
        // being hoisted back into a whole-pass fast path: a saturated pool
        // does pay for an admission evaluation it then discards on every
        // non-parked, non-churned candidate, but hoisting it above the pause
        // gate would stop `DispatchEvent::DispatchHeldByPause` firing for
        // those rows when a pause and a full pool coincide, and that event
        // is a deliberate diagnostic this sweep must not silently drop.
        if !coordinator.worker_pool().has_idle_worker().await {
            outcome.no_worker_skipped += 1;
            continue;
        }

        // Request a fresh execution. The `is_live` closure treats an
        // execution as live only if a worker slot currently claims it.
        // A non-terminal execution that is NOT claimed means the worker
        // died without updating the DB — `request_execution_with_live_check`
        // will mark it `abandoned` and create a new `ready` row.
        //
        // This is the redispatch path a dead-pid reap actually lands on
        // (`dead_pid_sweep::reap_dead_execution` marks the execution
        // `orphaned` and releases the slot; it does not itself mint a
        // successor). Mirror `rescan_active_dispatch`'s orphan handoff here
        // so a reaped item's successor inherits the dead worker's dirty
        // workspace instead of starting clean and losing the recovery
        // patch.
        let latest_execution = work_db.latest_execution_for_work_item(&work_item_id).ok().flatten();
        let is_orphaned_predecessor = latest_execution
            .as_ref()
            .is_some_and(|prev| prev.status == ExecutionStatus::Orphaned);
        let preferred_workspace_id = latest_execution
            .as_ref()
            .filter(|_| is_orphaned_predecessor)
            .and_then(|prev| prev.cube_workspace_id.clone());
        let is_live = |exec_id: &str| claimed.contains(exec_id);
        let new_execution = match work_db.request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .maybe_preferred_workspace_id(preferred_workspace_id)
                .allow_dirty(is_orphaned_predecessor)
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
mod tests;
