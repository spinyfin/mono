//! The `automation_scheduler` interval loop (Maint task 5).
//!
//! Each tick, for every enabled `schedule`-triggered automation that is due,
//! this loop decides what to do with the occurrence and records the decision
//! in `automation_runs`. The actual triage *execution* (creating the
//! `automation_triage` work_execution and rendering the preamble) is Maint
//! task 6 and is reached only through the [`TriageDispatcher`] seam — this
//! task ships the decision engine, the occurrence math (see
//! [`crate::automation_schedule`]), and the run-history writes.
//!
//! ## Per-tick decision, in order
//!
//! 1. **Initialise** — `next_due_at IS NULL` (never scheduled): compute the
//!    next occurrence and park it; do not fire this tick.
//! 2. **Not due** — `now < next_due_at`: nothing to do.
//! 3. **Catch-up collapse** — walk forward past every occurrence `<= now`,
//!    so a backlog accumulated while the laptop was asleep collapses to the
//!    single most-recent occurrence instead of firing a stampede.
//! 4. **Skip-if-stale** — if that most-recent occurrence is older than the
//!    catch-up window *and we have never attempted it*, it is stale: record
//!    a `skipped` run and advance to the next occurrence.
//! 5. **Already recorded** — an occurrence that already carries a terminal
//!    outcome or a live triage execution is owned elsewhere; advance past it
//!    rather than dispatching it a second time.
//! 6. **Retry pacing** — an occurrence held for retry gets its own deadline
//!    ([`AUTOMATION_RETRY_HOLD_MAX_SECS`]) and its own cadence
//!    ([`AUTOMATION_RETRY_INTERVAL_SECS`]); past the deadline it is finalised
//!    `failed_gave_up` and the schedule advances.
//! 7. **Open-task gate** — if the automation is already at its
//!    `open_task_limit`, record `suppressed_at_limit` and advance (so a
//!    capped automation doesn't fire a backlog the instant a task merges).
//! 8. **Fire** — dispatch triage. On success the occurrence is recorded as
//!    `failed_will_retry` (the pessimistic default; the task-6 detector
//!    flips it once the worker reaches a decision) and the schedule
//!    advances. On a transient pre-start failure the occurrence is *held*
//!    (`next_due_at` unchanged) for retry. On a [`TriageDispatch::Held`] the
//!    occurrence is held with *no* run row written at all.
//!
//! ## Holding is not failing, and neither is pausing
//!
//! Three distinct things used to share the single `TransientFailure` /
//! `failed_will_retry` spelling, and conflating them produced two defects:
//!
//! - **A global pause** (`bossctl automation pause`) is a human-held gate, not
//!   a failure. It is [`TriageDispatch::Held`]: no `automation_runs` row, no
//!   staleness clock, no `next_due_at` movement. The loop does not even
//!   evaluate while paused — see [`spawn_loop`] — because an early return from
//!   the pass alone would still spin: the un-advanced `next_due_at` is in the
//!   past, and the sleep computation used to floor a past-due occurrence at one
//!   second. Both halves are required.
//! - **A genuine transient pre-start failure** (VPN down, repo unresolvable) is
//!   [`TriageDispatch::TransientFailure`] and *is* recorded — but it is paced
//!   (step 6) rather than retried every second, and it is exempt from the
//!   catch-up window.
//! - **A missed occurrence** (the engine was down) is what the catch-up window
//!   in step 4 is actually for. Applying it to a held occurrence made the two
//!   fight: the hold said "retry me later", staleness said "you held too long",
//!   and any transient blocker outlasting 15 minutes silently lost its
//!   occurrence. Staleness now applies only to occurrences never attempted.
//!
//! ## A deliberate refinement of the design's skip rule
//!
//! The design (`maintenance-tasks.md` §"Scheduling semantics" step 3)
//! phrases skip-if-imminent as `following - now <= catch_up_window`. Taken
//! literally that is degenerate for sub-window cron periods: an
//! every-5-minute job would have `following - now ≈ 5min <= 15min` on *every*
//! tick and would skip every fire. We implement the equivalent-intent rule
//! `staleness = now - most_recent_occurrence > catch_up_window`, which
//! reproduces all of the design's worked examples (a daily 2pm job missed
//! until 1:50pm next day correctly skips to the real 2pm; a 10-minute-late
//! wake catches up) and is correct across all cron frequencies.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use async_trait::async_trait;
use boss_protocol::{
    AUTOMATION_OUTCOME_FAILED_GAVE_UP, AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AUTOMATION_OUTCOME_SKIPPED,
    AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT, Automation, AutomationRun, AutomationTrigger,
};

use crate::automation_schedule::{next_occurrence_after, parse_cron, parse_timezone};
use crate::work::{AutomationFireRecord, WorkDb};

/// Maximum time the scheduler sleeps between passes. Caps the sleep so
/// the loop wakes at least hourly as a safety net, even with no automations
/// or all automations scheduled far in the future.
pub const AUTOMATION_SCHEDULER_MAX_SLEEP_SECS: u64 = 3600;

/// Sleep interval used when enabled automations have an uninitialized
/// `next_due_at` (i.e., created/updated but not yet seen by a scheduler
/// pass). Short so a freshly-created automation's first occurrence is
/// computed promptly even when no kick arrives before the scheduler's
/// current sleep expires.
pub const AUTOMATION_SCHEDULER_UNINITIALIZED_POLL_SECS: u64 = 5;

/// Default catch-up window: an occurrence missed by more than this is
/// considered stale and skipped. 15 minutes per the design — long enough
/// that a brief sleep/wake doesn't lose a daily job, short enough that a
/// "2pm weekday" job missed until ~2pm next day skips to the real 2pm.
/// Overridable per automation via `automations.catch_up_window_secs`.
pub const DEFAULT_CATCH_UP_WINDOW_SECS: i64 = 15 * 60;

/// Upper bound on how many occurrences the catch-up collapse will walk in a
/// single tick. Protects against a pathological high-frequency cron after a
/// very long outage (e.g. an every-minute job offline for weeks); such a
/// case converges over a few ticks instead of doing unbounded work in one.
const MAX_CATCH_UP_COLLAPSE: u32 = 10_000;

/// Minimum gap between two dispatch attempts for the *same* held occurrence.
///
/// Before this existed the retry cadence was whatever the loop's sleep floor
/// happened to be — one second — so a blocker lasting minutes produced hundreds
/// of `automation_runs` upsert transactions and a full re-run of the
/// per-occurrence ladder (cron parse, timezone parse, catch-up collapse,
/// open-task count) for each one. A pre-start blocker that clears at all clears
/// on a scale of minutes, not seconds; one attempt per minute finds it
/// essentially as fast at 1/60th the churn.
pub const AUTOMATION_RETRY_INTERVAL_SECS: i64 = 60;

/// How long past an occurrence's scheduled time we keep retrying it after a
/// transient pre-start failure before giving up and advancing the schedule.
///
/// This is deliberately *not* the catch-up window. The catch-up window answers
/// "was this occurrence missed because the engine was down?", which an
/// occurrence we have already attempted demonstrably was not. Sharing the
/// window meant every transient blocker outlasting 15 minutes — a VPN down for
/// 20 — had its occurrence quietly relabelled and dropped while the run row
/// still read `failed_will_retry`, promising a retry that would never come.
///
/// The effective deadline is `max(catch_up_window, this)`, measured from the
/// occurrence's *first* dispatch attempt (`automation_runs.first_attempted_at`)
/// — not from the occurrence's scheduled time. Measuring from the scheduled
/// time instead would let a late first attempt eat into the budget (worse,
/// the longer the custom window, the less budget would be left), which is
/// exactly backwards: an automation configured with a long custom catch-up
/// window never gets a *shorter* retry budget than its own tolerance for
/// lateness.
pub const AUTOMATION_RETRY_HOLD_MAX_SECS: i64 = 3600;

/// How soon to re-run a pass after a pass-level error (e.g. the due-automation
/// query failed). Short, because the failure is usually momentary and nothing
/// else will wake the loop.
const AUTOMATION_SCHEDULER_ERROR_RETRY_SECS: i64 = 5;

/// Result of attempting to dispatch a triage execution for a fired
/// occurrence. The actual execution machinery is Maint task 6; this enum is
/// the seam the scheduler decides `advance` vs `hold` on.
#[derive(Debug, Clone)]
pub enum TriageDispatch {
    /// A triage `work_execution` was created and enqueued. The occurrence is
    /// recorded `failed_will_retry` (pessimistic) with this execution id and
    /// the schedule advances; the task-6 detector finalises the outcome.
    Dispatched { execution_id: String },
    /// A transient pre-start failure (cube lease error, git remote
    /// unreachable, product repo unresolvable). The occurrence is held for
    /// retry — `next_due_at` is not advanced — and recorded
    /// `failed_will_retry` so an operator can see the blocker. Retries are
    /// paced by [`AUTOMATION_RETRY_INTERVAL_SECS`] and bounded by
    /// [`AUTOMATION_RETRY_HOLD_MAX_SECS`].
    TransientFailure { detail: String },
    /// Dispatch is gated by a deliberate operator action — today, the global
    /// automation pause. Distinct from [`Self::TransientFailure`] in three
    /// ways that all matter:
    ///
    /// 1. **No run row is written.** A pause is not a failure, and recording
    ///    one `failed_will_retry` row per attempt is what turned a 90-hour
    ///    pause into ~790,000 upsert transactions.
    /// 2. **No staleness clock starts.** The occurrence is neither attempted
    ///    nor missed; when the operator resumes, it is evaluated on its
    ///    merits (and, if genuinely stale by then, skipped exactly once).
    /// 3. **Nothing will change until a human acts**, so the scheduler does
    ///    not schedule a retry — it waits for the resume kick.
    Held { reason: String },
}

/// The fire seam. The scheduler calls this when an occurrence is due, under
/// cap, and not stale. Implemented for real in Maint task 6; task 5 wires
/// [`LoggingTriageDispatcher`].
#[async_trait]
pub trait TriageDispatcher: Send + Sync {
    async fn dispatch_triage(&self, automation: &Automation, scheduled_for_epoch: i64) -> TriageDispatch;
}

/// Non-dispatching fallback used when no real triage dispatcher is wired,
/// superseded in production by
/// [`crate::automation_triage::EngineTriageDispatcher`]. Every fire reports a
/// transient failure, so the occurrence is *held* (recorded
/// `failed_will_retry`, schedule not advanced) rather than silently dropped —
/// the same state a real VPN-down pre-start failure produces, including the
/// paced retries and the [`AUTOMATION_RETRY_HOLD_MAX_SECS`] deadline after
/// which it is abandoned as `failed_gave_up`. With zero automations configured
/// the loop is inert; the first time a real automation comes due this logs a
/// single warning naming the missing piece.
#[derive(Debug, Default)]
pub struct LoggingTriageDispatcher;

#[async_trait]
impl TriageDispatcher for LoggingTriageDispatcher {
    async fn dispatch_triage(&self, automation: &Automation, scheduled_for_epoch: i64) -> TriageDispatch {
        tracing::warn!(
            automation_id = %automation.id,
            scheduled_for = scheduled_for_epoch,
            "automation due to fire, but triage dispatch is not yet implemented \
             (Maint task 6); holding occurrence as failed_will_retry",
        );
        TriageDispatch::TransientFailure {
            detail: "triage dispatch not yet implemented (Maint task 6)".to_owned(),
        }
    }
}

/// Per-pass counters, for logging and tests. Constructed via `default()`
/// and incremented in place; the `bon::Builder` derive is present only to
/// satisfy the repo's giant-struct convention (`checkleft`'s
/// rust-giant-structs-use-builder, which flags 6+ named fields) — the
/// scheduler never builds one.
#[derive(Debug, Default, PartialEq, Eq, bon::Builder)]
pub struct AutomationSchedulerPass {
    /// Due automations evaluated this pass.
    pub evaluated: usize,
    /// Automations whose `next_due_at` was initialised this pass (no fire).
    pub initialized: usize,
    /// Occurrences fired (triage dispatched).
    pub fired: usize,
    /// Occurrences suppressed at the open-task limit.
    pub suppressed: usize,
    /// Stale occurrences skipped.
    pub skipped_stale: usize,
    /// Fires held after a transient dispatch failure.
    pub held_transient: usize,
    /// Fires held because dispatch is operator-gated ([`TriageDispatch::Held`]).
    /// No `automation_runs` row is written for these.
    pub held_gated: usize,
    /// Held occurrences left alone this pass because their retry interval had
    /// not elapsed yet.
    pub retry_deferred: usize,
    /// Held occurrences abandoned this pass after exhausting their retry
    /// deadline (recorded `failed_gave_up`, schedule advanced).
    pub gave_up: usize,
    /// Automations skipped this pass due to a malformed cron/timezone.
    pub config_errors: usize,
    /// Earliest epoch (UTC seconds) at which running another pass could
    /// produce a *different* decision — a paced retry becoming eligible, or an
    /// advance that landed on an occurrence still in the past (the catch-up
    /// collapse cap). `None` means nothing this pass wants an early wake, so
    /// the loop sleeps until the next future `next_due_at`.
    ///
    /// This exists because the sleep computation no longer floors a past-due
    /// occurrence at one second: it deliberately ignores occurrences already
    /// at or before `now`, so the pass has to say when to look again.
    pub wake_hint: Option<i64>,
}

impl AutomationSchedulerPass {
    /// Ask the loop to wake no later than `epoch`. Keeps the earliest request
    /// across all automations evaluated this pass.
    fn wake_no_later_than(&mut self, epoch: i64) {
        self.wake_hint = Some(match self.wake_hint {
            Some(existing) => existing.min(epoch),
            None => epoch,
        });
    }

    /// Note that the schedule advanced to `advance_to`. When the new
    /// occurrence is *itself* already in the past (a long outage whose backlog
    /// exceeded [`MAX_CATCH_UP_COLLAPSE`] in one pass), ask for an immediate
    /// re-tick so the backlog still converges over a few quick passes.
    fn note_advance(&mut self, now: i64, advance_to: Option<i64>) {
        if advance_to.is_some_and(|next| next <= now) {
            self.wake_no_later_than(now);
        }
    }
}

/// Whether this run row represents an occurrence still held for retry: the
/// scheduler recorded a transient pre-start failure, no triage worker was ever
/// created for it, and nothing has finalised it since.
///
/// The `triage_execution_id` check is what keeps a *dispatched* occurrence out
/// of the retry path: a fired run also carries the pessimistic
/// `failed_will_retry` outcome with a NULL `finished_at` until the outcome
/// detector flips it, and re-dispatching one would spawn a duplicate triage
/// worker for the same occurrence.
fn is_retryable_hold(run: &AutomationRun) -> bool {
    run.outcome == AUTOMATION_OUTCOME_FAILED_WILL_RETRY
        && run.finished_at.is_none()
        && run.triage_execution_id.is_none()
}

/// Spawn the scheduler loop. Fires immediately on boot (so a daily job whose
/// occurrence elapsed while the engine was down is caught up without waiting
/// a full interval) then sleeps until the earliest of:
///
/// - The minimum `next_due_at` across all enabled automations (event-driven
///   wake: the loop wakes exactly when the next automation is due rather than
///   polling on a fixed coarse interval).
/// - [`AUTOMATION_SCHEDULER_MAX_SLEEP_SECS`] (safety-net heartbeat for the
///   no-automations and far-future-fire cases).
/// - [`AUTOMATION_SCHEDULER_UNINITIALIZED_POLL_SECS`] when any enabled
///   automation still has `next_due_at IS NULL` (bootstrap case: initialise
///   the first occurrence promptly).
/// - Immediate wake via `kick.notify_one()`, called by automation mutation
///   handlers (create, update, enable, disable, delete) and by the
///   pause/resume handler so the scheduler recomputes its sleep on every
///   state change without waiting out the current interval.
///
/// ## While globally paused, the loop does not evaluate at all
///
/// `is_paused` is the same `ExecutionCoordinator::is_automation_paused` flag
/// [`crate::automation_triage::EngineTriageDispatcher`] consults. Checking it
/// *here*, ahead of `run_one_pass`, is what makes a pause actually quiet:
/// checking it only at the dispatch seam left the full per-occurrence ladder
/// and one `automation_runs` write transaction running for every due
/// automation, every second, for the entire pause.
///
/// This mirrors `enabled = 0`, which has always been handled correctly — and
/// correctly in *both* halves: `list_due_automations` skips a disabled
/// automation, and `list_min_future_next_due_at_for_scheduler` keeps it from
/// influencing the sleep. A pause needs both halves too, which is why the
/// sleep is short-circuited below rather than only the pass.
///
/// **Resume latency depends on the kick.** Because a paused loop sleeps up to
/// [`AUTOMATION_SCHEDULER_MAX_SLEEP_SECS`], `handle_set_automation_paused`
/// must notify `kick` when it clears the flag; without it, resuming would take
/// up to an hour to take effect.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    dispatcher: Arc<dyn TriageDispatcher>,
    kick: Arc<Notify>,
    is_paused: Arc<dyn Fn() -> bool + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut was_paused = false;
        loop {
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            let paused = is_paused();
            if paused != was_paused {
                if paused {
                    tracing::info!(
                        "automation scheduler: globally paused — suspending evaluation until resume \
                         (no occurrences are evaluated and no runs are recorded while paused)",
                    );
                } else {
                    tracing::info!("automation scheduler: resumed — evaluating due automations");
                }
                was_paused = paused;
            }

            let pass = if paused {
                None
            } else {
                let pass = run_one_pass(work_db.as_ref(), now, dispatcher.as_ref()).await;
                if pass.evaluated > 0 {
                    tracing::info!(
                        evaluated = pass.evaluated,
                        initialized = pass.initialized,
                        fired = pass.fired,
                        suppressed = pass.suppressed,
                        skipped_stale = pass.skipped_stale,
                        held_transient = pass.held_transient,
                        held_gated = pass.held_gated,
                        retry_deferred = pass.retry_deferred,
                        gave_up = pass.gave_up,
                        config_errors = pass.config_errors,
                        "automation scheduler: pass complete",
                    );
                }
                Some(pass)
            };

            let mut sleep_secs = next_sleep_secs(work_db.as_ref(), now, paused);
            if let Some(wake) = pass.and_then(|p| p.wake_hint) {
                let wait = (wake - now).clamp(1, AUTOMATION_SCHEDULER_MAX_SLEEP_SECS as i64) as u64;
                sleep_secs = sleep_secs.min(wait);
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {}
                _ = kick.notified() => {}
            }
        }
    })
}

/// Compute how many seconds the scheduler should sleep before its next pass.
///
/// Returns the number of seconds until the earliest *future* `next_due_at`
/// among all enabled `schedule` automations, clamped to
/// `[1, AUTOMATION_SCHEDULER_MAX_SLEEP_SECS]`. Falls back to
/// `AUTOMATION_SCHEDULER_UNINITIALIZED_POLL_SECS` when any automation is
/// still uninitialized, and to `AUTOMATION_SCHEDULER_MAX_SLEEP_SECS` when
/// no enabled automations exist.
///
/// `paused` short-circuits to the maximum sleep. This is load-bearing, not
/// belt-and-braces: while paused, `next_due_at` is never advanced, so every
/// due automation's occurrence sits permanently in the past. Skipping the pass
/// without also silencing the sleep would leave the loop waking, re-querying
/// and re-sleeping every second for the whole pause.
///
/// Occurrences already at or before `now` no longer floor the sleep at one
/// second either. After a pass has run, a still-past-due occurrence is one the
/// pass could not act on — held for retry, or config-broken — and re-deciding
/// it a second later cannot help. Those cases pace themselves through
/// [`AutomationSchedulerPass::wake_hint`]; a config error waits for the kick
/// that any automation edit already fires.
pub(crate) fn next_sleep_secs(work_db: &WorkDb, now: i64, paused: bool) -> u64 {
    if paused {
        return AUTOMATION_SCHEDULER_MAX_SLEEP_SECS;
    }
    match work_db.list_min_future_next_due_at_for_scheduler(now) {
        Err(_) => AUTOMATION_SCHEDULER_UNINITIALIZED_POLL_SECS,
        Ok((_, true)) => AUTOMATION_SCHEDULER_UNINITIALIZED_POLL_SECS,
        Ok((None, false)) => AUTOMATION_SCHEDULER_MAX_SLEEP_SECS,
        Ok((Some(min_next_due), _)) => (min_next_due - now).clamp(1, AUTOMATION_SCHEDULER_MAX_SLEEP_SECS as i64) as u64,
    }
}

/// Run a single scheduler pass against `now_epoch` (UTC seconds). Pure of
/// wall-clock reads so DST and catch-up behaviour is deterministically
/// testable.
pub async fn run_one_pass(
    work_db: &WorkDb,
    now_epoch: i64,
    dispatcher: &dyn TriageDispatcher,
) -> AutomationSchedulerPass {
    let mut pass = AutomationSchedulerPass::default();

    let due = match work_db.list_due_automations(now_epoch) {
        Ok(due) => due,
        Err(err) => {
            tracing::warn!(
                ?err,
                "automation scheduler: failed to list due automations; skipping pass"
            );
            // Nothing advanced, so the sleep computation has no future
            // occurrence to aim at. Ask for a prompt retry rather than
            // sleeping out the safety-net hour on a momentary DB error.
            pass.wake_no_later_than(now_epoch + AUTOMATION_SCHEDULER_ERROR_RETRY_SECS);
            return pass;
        }
    };

    for automation in due {
        pass.evaluated += 1;
        if let Err(err) = evaluate_one(work_db, now_epoch, dispatcher, &automation, &mut pass).await {
            tracing::warn!(
                automation_id = %automation.id,
                ?err,
                "automation scheduler: error evaluating automation; skipping",
            );
            pass.wake_no_later_than(now_epoch + AUTOMATION_SCHEDULER_ERROR_RETRY_SECS);
        }
    }

    pass
}

async fn evaluate_one(
    work_db: &WorkDb,
    now: i64,
    dispatcher: &dyn TriageDispatcher,
    automation: &Automation,
    pass: &mut AutomationSchedulerPass,
) -> anyhow::Result<()> {
    let AutomationTrigger::Schedule { cron, timezone } = &automation.trigger;

    let schedule = match parse_cron(cron) {
        Ok(schedule) => schedule,
        Err(err) => {
            tracing::warn!(automation_id = %automation.id, cron = %cron, %err, "invalid cron");
            pass.config_errors += 1;
            return Ok(());
        }
    };
    let tz = match parse_timezone(timezone) {
        Ok(tz) => tz,
        Err(err) => {
            tracing::warn!(automation_id = %automation.id, timezone = %timezone, %err, "invalid timezone");
            pass.config_errors += 1;
            return Ok(());
        }
    };

    // 1. Initialise next_due_at if unset (or unparseable).
    let next_due = match automation.next_due_at.as_deref().and_then(|s| s.parse::<i64>().ok()) {
        Some(next_due) => next_due,
        None => {
            match next_occurrence_after(&schedule, tz, now) {
                Some(next) => {
                    work_db.initialize_automation_next_due_at(&automation.id, next)?;
                    pass.initialized += 1;
                }
                None => tracing::warn!(
                    automation_id = %automation.id,
                    "no cron occurrence within scan horizon; leaving next_due_at unset",
                ),
            }
            return Ok(());
        }
    };

    // 2. Not actually due (list query is inclusive; guard against clock skew).
    if now < next_due {
        return Ok(());
    }

    // 3. Catch-up collapse: most_recent = latest occurrence <= now;
    //    following = first occurrence strictly after now.
    let mut most_recent = next_due;
    let mut following = next_occurrence_after(&schedule, tz, most_recent);
    let mut collapsed = 0u32;
    while let Some(f) = following {
        if f <= now && collapsed < MAX_CATCH_UP_COLLAPSE {
            most_recent = f;
            collapsed += 1;
            following = next_occurrence_after(&schedule, tz, most_recent);
        } else {
            break;
        }
    }

    let catch_up_window = automation.catch_up_window_secs.unwrap_or(DEFAULT_CATCH_UP_WINDOW_SECS);
    let staleness = now - most_recent;

    // Anything already recorded against this occurrence decides which of the
    // three deadlines below applies. One lookup serves steps 4, 5 and 6.
    let existing_run = work_db.automation_run_for_occurrence(&automation.id, most_recent)?;

    // 4. Skip-if-stale — for occurrences we have never attempted.
    //
    //    The catch-up window is a *missed-while-down* test, so it only makes
    //    sense for an occurrence the scheduler never got to. Applying it to a
    //    held occurrence is the "hold and catch-up fight each other" defect:
    //    the hold asked to be retried, the window relabelled it
    //    `failed_will_retry` with "stale: catch-up window elapsed before
    //    retry" and advanced past it — a drop dressed as a retry. Held
    //    occurrences are governed by step 6 instead.
    if existing_run.is_none() && staleness > catch_up_window {
        let Some(advance_to) = following else {
            tracing::warn!(
                automation_id = %automation.id,
                "stale occurrence but no following occurrence within horizon; holding",
            );
            return Ok(());
        };
        work_db.record_automation_run_and_advance(
            AutomationFireRecord::builder()
                .automation_id(automation.id.clone())
                .scheduled_for(most_recent)
                .started_at(now)
                .outcome(AUTOMATION_OUTCOME_SKIPPED)
                .finished_at(now)
                .detail(format!(
                    "stale catch-up: occurrence was {staleness}s late (> catch-up window {catch_up_window}s); advanced to next"
                ))
                .next_due_at(advance_to)
                .build(),
        )?;
        pass.skipped_stale += 1;
        pass.note_advance(now, Some(advance_to));

        // The collapse above may have walked `most_recent` straight past an
        // earlier occurrence still held for retry (no pass ran between the
        // hold and this one). That row's `automation_run_for_occurrence`
        // lookup above was for the *new* `most_recent`, so it was never
        // consulted and would otherwise be stranded at `failed_will_retry`
        // forever. Finalise it honestly now that we know it will never be
        // retried.
        let stranded = work_db.finalize_stale_retry_holds_before(
            &automation.id,
            most_recent,
            now,
            &format!("occurrence was superseded by a later catch-up collapse to {most_recent}"),
        )?;
        pass.gave_up += stranded;
        return Ok(());
    }

    if let Some(run) = &existing_run {
        // 5. Already dispatched, or already finalised with some other
        //    outcome. Either way this occurrence is not ours to fire again —
        //    re-dispatching would spawn a second triage worker for it. Just
        //    advance past it. Only reachable when `following` was None at the
        //    time the row was written (no occurrence inside the cron scan
        //    horizon), so the schedule could not advance then.
        if !is_retryable_hold(run) {
            if let Some(advance_to) = following {
                work_db.initialize_automation_next_due_at(&automation.id, advance_to)?;
                pass.note_advance(now, Some(advance_to));
            }
            return Ok(());
        }

        // 6. A held occurrence, mid-retry.
        //
        //    The deadline is measured from the *first* attempt, not from the
        //    occurrence's scheduled time: `staleness` above already includes
        //    however late that first attempt landed, so comparing it directly
        //    against `retry_deadline` would silently eat into the retry
        //    budget whenever the first attempt was itself late inside a long
        //    custom catch-up window — the longer the window, the shorter the
        //    budget, inverting the guarantee below. Measuring from
        //    `first_attempted_at` keeps the retry budget exactly
        //    `retry_deadline` regardless of how late the first attempt was.
        let first_attempt = run
            .first_attempted_at
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(most_recent);
        let attempt_staleness = now - first_attempt;
        let retry_deadline = catch_up_window.max(AUTOMATION_RETRY_HOLD_MAX_SECS);
        if attempt_staleness > retry_deadline {
            let Some(advance_to) = following else {
                tracing::warn!(
                    automation_id = %automation.id,
                    "retry deadline elapsed but no following occurrence within horizon; holding",
                );
                return Ok(());
            };
            // Honest terminal state. The old code wrote `failed_will_retry`
            // here while advancing past the occurrence, so the run history
            // promised a retry that could never happen.
            work_db.record_automation_run_and_advance(
                AutomationFireRecord::builder()
                    .automation_id(automation.id.clone())
                    .scheduled_for(most_recent)
                    .started_at(now)
                    .outcome(AUTOMATION_OUTCOME_FAILED_GAVE_UP)
                    .finished_at(now)
                    .detail(format!(
                        "gave up after retrying for {attempt_staleness}s since the first attempt \
                         (> retry deadline {retry_deadline}s); last failure: {}",
                        run.detail.as_deref().unwrap_or("unknown")
                    ))
                    .next_due_at(advance_to)
                    .build(),
            )?;
            pass.gave_up += 1;
            pass.note_advance(now, Some(advance_to));
            return Ok(());
        }

        // Pace the retry. `started_at` is rewritten on every attempt by the
        // upsert, so it is the time of the most recent attempt.
        let last_attempt = run.started_at.parse::<i64>().unwrap_or(most_recent);
        let retry_at = last_attempt.saturating_add(AUTOMATION_RETRY_INTERVAL_SECS);
        if now < retry_at {
            pass.retry_deferred += 1;
            pass.wake_no_later_than(retry_at);
            return Ok(());
        }
    }

    // 7. Open-task-limit gate.
    let open = work_db.count_open_tasks_for_automation(&automation.id)?;
    if open >= automation.open_task_limit {
        // Advance past the suppressed occurrence so a freshly-merged
        // automation doesn't fire its whole backlog at once. If there's no
        // following occurrence, hold (don't advance) rather than lose the slot.
        work_db.record_automation_run_and_advance(
            AutomationFireRecord::builder()
                .automation_id(automation.id.clone())
                .scheduled_for(most_recent)
                .started_at(now)
                .outcome(AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT)
                .finished_at(now)
                .detail(format!(
                    "open-task count {open} at limit {}",
                    automation.open_task_limit
                ))
                .maybe_next_due_at(following)
                .build(),
        )?;
        pass.suppressed += 1;
        pass.note_advance(now, following);
        return Ok(());
    }

    // 8. Fire.
    match dispatcher.dispatch_triage(automation, most_recent).await {
        TriageDispatch::Dispatched { execution_id } => {
            // Record the pessimistic `failed_will_retry` default now; the
            // task-6 outcome detector overwrites both `outcome` and `detail`
            // when the triage worker's Stop fires. Seed a placeholder detail so
            // a row left in this state (worker crashed/hung and never reached
            // Stop, so `finished_at` is also still NULL) is distinguishable in
            // the run history from a run that finalised with a real outcome —
            // previously such rows carried an empty detail that gave the
            // operator nothing to act on.
            work_db.record_automation_run_and_advance(
                AutomationFireRecord::builder()
                    .automation_id(automation.id.clone())
                    .scheduled_for(most_recent)
                    .started_at(now)
                    .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                    .detail("dispatched; awaiting triage worker decision (Stop not yet received)")
                    .triage_execution_id(execution_id)
                    .maybe_next_due_at(following)
                    .build(),
            )?;
            pass.fired += 1;
            pass.note_advance(now, following);
        }
        TriageDispatch::TransientFailure { detail } => {
            // Hold the occurrence (next_due_at unchanged) so it is retried.
            work_db.record_automation_run_and_advance(
                AutomationFireRecord::builder()
                    .automation_id(automation.id.clone())
                    .scheduled_for(most_recent)
                    .started_at(now)
                    .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                    .detail(detail)
                    .build(),
            )?;
            pass.held_transient += 1;
            // The occurrence stays in the past, which the sleep computation
            // now ignores — so the retry has to be scheduled explicitly.
            pass.wake_no_later_than(now + AUTOMATION_RETRY_INTERVAL_SECS);
        }
        TriageDispatch::Held { reason } => {
            // An operator-held gate: record nothing, advance nothing, start no
            // staleness clock, and schedule no retry. Only a human clearing the
            // gate changes the answer, and doing so kicks the scheduler.
            tracing::debug!(
                automation_id = %automation.id,
                scheduled_for = most_recent,
                %reason,
                "automation scheduler: dispatch gated; holding occurrence without recording a run",
            );
            pass.held_gated += 1;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::automation_schedule::next_occurrence_after_str;
    use crate::test_support::*;
    use crate::work::WorkDb;
    use boss_protocol::{
        AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AUTOMATION_OUTCOME_SKIPPED, AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT,
        AutomationPatch, AutomationTrigger, CreateAutomationInput,
    };

    /// A dispatcher with a fixed verdict, recording every call.
    struct FakeDispatcher {
        verdict: TriageDispatch,
        calls: Mutex<Vec<(String, i64)>>,
    }

    impl FakeDispatcher {
        fn dispatched() -> Self {
            Self {
                verdict: TriageDispatch::Dispatched {
                    execution_id: "exec_test".to_owned(),
                },
                calls: Mutex::new(Vec::new()),
            }
        }
        fn transient() -> Self {
            Self {
                verdict: TriageDispatch::TransientFailure {
                    detail: "vpn down".to_owned(),
                },
                calls: Mutex::new(Vec::new()),
            }
        }
        fn held() -> Self {
            Self {
                verdict: TriageDispatch::Held {
                    reason: "automation is paused".to_owned(),
                },
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl TriageDispatcher for FakeDispatcher {
        async fn dispatch_triage(&self, a: &Automation, scheduled_for: i64) -> TriageDispatch {
            self.calls.lock().unwrap().push((a.id.clone(), scheduled_for));
            self.verdict.clone()
        }
    }

    /// Create a daily-2pm-UTC automation. `open_task_limit` default 1.
    fn create_daily_automation(db: &WorkDb, product_id: &str) -> Automation {
        seed_daily_automation(db, product_id)
    }

    fn utc_epoch(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp()
    }

    /// An open task counted against the automation's cap.
    fn create_open_task_for_automation(db: &WorkDb, product_id: &str, automation_id: &str) {
        let task_id = create_test_chore_manual(db, product_id, "produced").id;
        db.stamp_task_source_automation_for_test(&task_id, automation_id, "todo")
            .unwrap();
    }

    #[tokio::test]
    async fn first_evaluation_initializes_next_due_without_firing() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        assert!(automation.next_due_at.is_none());

        let now = utc_epoch(2026, 5, 28, 10, 0);
        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.initialized, 1);
        assert_eq!(pass.fired, 0);
        assert_eq!(dispatcher.call_count(), 0);

        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        let next: i64 = reloaded.next_due_at.unwrap().parse().unwrap();
        assert_eq!(next, utc_epoch(2026, 5, 28, 14, 0)); // today 2pm
        // No run recorded for an initialisation.
        assert!(db.list_automation_runs(&automation.id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn on_time_fire_dispatches_and_advances() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        // Park next_due at 2pm; fire 5s later.
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 28, 14, 0))
            .unwrap();
        let now = utc_epoch(2026, 5, 28, 14, 0) + 5;

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.fired, 1, "{pass:?}");
        assert_eq!(dispatcher.call_count(), 1);

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_WILL_RETRY);
        assert_eq!(runs[0].triage_execution_id.as_deref(), Some("exec_test"));
        assert_eq!(
            runs[0].scheduled_for.parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 28, 14, 0)
        );

        // next_due advanced to tomorrow 2pm.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 29, 14, 0)
        );
        assert_eq!(
            reloaded.last_outcome.as_deref(),
            Some(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
        );
    }

    #[tokio::test]
    async fn transient_failure_holds_occurrence() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();
        let now = occ + 5;

        let dispatcher = FakeDispatcher::transient();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.held_transient, 1, "{pass:?}");
        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_WILL_RETRY);
        assert!(runs[0].triage_execution_id.is_none());

        // next_due NOT advanced — occurrence is held.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(reloaded.next_due_at.unwrap().parse::<i64>().unwrap(), occ);

        // A later pass re-attempts the SAME occurrence and upserts (no dup
        // row). Retries are paced by AUTOMATION_RETRY_INTERVAL_SECS, so the
        // re-attempt has to be at least that far past the first one.
        let pass2 = run_one_pass(&db, now + AUTOMATION_RETRY_INTERVAL_SECS, &dispatcher).await;
        assert_eq!(pass2.held_transient, 1);
        let runs2 = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs2.len(), 1, "transient retry must upsert, not duplicate");
    }

    /// The retry cadence is paced: a pass arriving before
    /// `AUTOMATION_RETRY_INTERVAL_SECS` has elapsed since the last attempt does
    /// not re-dispatch and does not rewrite the run row. Before this, the loop
    /// re-ran the whole per-occurrence ladder and wrote an upsert transaction
    /// every second for as long as the blocker lasted.
    #[tokio::test]
    async fn held_occurrence_retries_are_paced() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let dispatcher = FakeDispatcher::transient();
        let first = run_one_pass(&db, occ + 5, &dispatcher).await;
        assert_eq!(first.held_transient, 1);
        assert_eq!(dispatcher.call_count(), 1);
        assert_eq!(
            first.wake_hint,
            Some(occ + 5 + AUTOMATION_RETRY_INTERVAL_SECS),
            "a fresh hold must schedule its own retry — the sleep computation ignores past-due rows",
        );

        // One second later: deferred, no dispatch, no write.
        let soon = run_one_pass(&db, occ + 6, &dispatcher).await;
        assert_eq!(soon.retry_deferred, 1, "{soon:?}");
        assert_eq!(soon.held_transient, 0);
        assert_eq!(
            dispatcher.call_count(),
            1,
            "must not re-dispatch inside the retry interval"
        );
        assert_eq!(
            soon.wake_hint,
            Some(occ + 5 + AUTOMATION_RETRY_INTERVAL_SECS),
            "the deferred retry paces the loop's next wake",
        );

        // Once the interval elapses, it retries.
        let later = run_one_pass(&db, occ + 5 + AUTOMATION_RETRY_INTERVAL_SECS, &dispatcher).await;
        assert_eq!(later.held_transient, 1, "{later:?}");
        assert_eq!(dispatcher.call_count(), 2);
    }

    /// A genuine transient failure outlasting the catch-up window keeps its
    /// occurrence. Previously the staleness check relabelled the held row
    /// "stale: catch-up window elapsed before retry" and advanced past it, so a
    /// VPN down for 20 minutes silently lost the occurrence — while the run
    /// history still read `failed_will_retry`, promising a retry that could
    /// never happen.
    #[tokio::test]
    async fn held_occurrence_survives_the_catch_up_window() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let dispatcher = FakeDispatcher::transient();
        run_one_pass(&db, occ + 5, &dispatcher).await;

        // 20 minutes later — past the 15-minute catch-up window, inside the
        // retry deadline. The blocker has cleared, so this attempt succeeds.
        let recovered = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, occ + 20 * 60, &recovered).await;

        assert_eq!(pass.skipped_stale, 0, "a held occurrence is not 'missed while down'");
        assert_eq!(pass.fired, 1, "{pass:?}");
        assert_eq!(recovered.call_count(), 1);

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1, "still the same occurrence, upserted");
        assert_eq!(
            runs[0].scheduled_for.parse::<i64>().unwrap(),
            occ,
            "the recovered fire must satisfy the original occurrence"
        );
        assert_eq!(runs[0].triage_execution_id.as_deref(), Some("exec_test"));
    }

    /// The retry loop is bounded, and it terminates honestly. Past the retry
    /// deadline the occurrence is recorded `failed_gave_up` — not
    /// `failed_will_retry`, which is what the old stale path wrote while
    /// advancing past the occurrence it claimed would be retried.
    #[tokio::test]
    async fn held_occurrence_gives_up_after_retry_deadline() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let dispatcher = FakeDispatcher::transient();
        // First attempt at occ + 5 — the retry deadline is measured from
        // *this*, not from `occ` itself.
        run_one_pass(&db, occ + 5, &dispatcher).await;
        let calls_after_first = dispatcher.call_count();

        let pass = run_one_pass(&db, occ + 5 + AUTOMATION_RETRY_HOLD_MAX_SECS + 1, &dispatcher).await;

        assert_eq!(pass.gave_up, 1, "{pass:?}");
        assert_eq!(
            dispatcher.call_count(),
            calls_after_first,
            "giving up must not dispatch again"
        );

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP);
        assert!(runs[0].finished_at.is_some(), "a terminal outcome is finished");
        assert!(
            runs[0].detail.as_deref().unwrap_or_default().contains("vpn down"),
            "the last failure must survive into the give-up detail: {:?}",
            runs[0].detail,
        );

        // Schedule advanced past the abandoned occurrence.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 29, 14, 0)
        );
    }

    /// An automation configured with a catch-up window longer than the default
    /// retry deadline keeps the longer of the two — its own tolerance for
    /// lateness is never shortened by the retry budget.
    #[tokio::test]
    async fn custom_catch_up_window_widens_the_retry_deadline() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let long_window = AUTOMATION_RETRY_HOLD_MAX_SECS * 3;
        let automation = db
            .create_automation(
                CreateAutomationInput::builder()
                    .product_id(product.clone())
                    .name("long-window")
                    .trigger(AutomationTrigger::Schedule {
                        cron: "0 14 * * *".to_owned(),
                        timezone: "UTC".to_owned(),
                    })
                    .standing_instruction("x")
                    .catch_up_window_secs(long_window)
                    .build(),
            )
            .unwrap();
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let dispatcher = FakeDispatcher::transient();
        run_one_pass(&db, occ + 5, &dispatcher).await;

        // Past the default deadline but inside this automation's window.
        let pass = run_one_pass(&db, occ + AUTOMATION_RETRY_HOLD_MAX_SECS + 60, &dispatcher).await;
        assert_eq!(pass.gave_up, 0, "{pass:?}");
        assert_eq!(pass.held_transient, 1, "still retrying inside the custom window");
    }

    /// The retry deadline is measured from the *first* dispatch attempt, not
    /// from the occurrence's scheduled time — even when that first attempt
    /// itself lands very late inside a long custom catch-up window.
    ///
    /// Before this was fixed, `retry_deadline` was compared against
    /// `now - most_recent` (staleness since the occurrence), so a first
    /// attempt landing near the end of a long window left almost no retry
    /// budget: with a 7200s window, a first attempt at `occ + 7100` would
    /// have given up after just ~101s, inverting the documented guarantee
    /// that a longer custom window never yields a *shorter* retry budget.
    #[tokio::test]
    async fn custom_catch_up_window_late_first_attempt_still_gets_the_full_retry_budget() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let long_window = 7200;
        let automation = db
            .create_automation(
                CreateAutomationInput::builder()
                    .product_id(product.clone())
                    .name("long-window-late-first-attempt")
                    .trigger(AutomationTrigger::Schedule {
                        cron: "0 14 * * *".to_owned(),
                        timezone: "UTC".to_owned(),
                    })
                    .standing_instruction("x")
                    .catch_up_window_secs(long_window)
                    .build(),
            )
            .unwrap();
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let dispatcher = FakeDispatcher::transient();
        // First attempt lands at occ + 7100 — inside the window (so not
        // "stale"), but only 100s before it elapses.
        let first_attempt_at = occ + 7100;
        let first = run_one_pass(&db, first_attempt_at, &dispatcher).await;
        assert_eq!(first.held_transient, 1, "{first:?}");
        assert_eq!(first.skipped_stale, 0, "still inside the catch-up window");

        // Just past the *occurrence's* staleness deadline (occ + 7200), but
        // only ~101s after the first attempt — well inside the retry budget
        // when measured correctly.
        let pass = run_one_pass(&db, first_attempt_at + 101, &dispatcher).await;
        assert_eq!(
            pass.gave_up, 0,
            "a late first attempt must not shrink the retry budget: {pass:?}"
        );
        assert_eq!(pass.held_transient, 1, "still retrying: {pass:?}");

        // The occurrence is only abandoned once the budget measured from the
        // first attempt — not the occurrence — actually elapses.
        let gave_up = run_one_pass(&db, first_attempt_at + long_window + 1, &dispatcher).await;
        assert_eq!(gave_up.gave_up, 1, "{gave_up:?}");
    }

    /// A held retry that predates a catch-up collapse the scheduler walks
    /// past is finalised as `failed_gave_up` rather than left stranded at
    /// `failed_will_retry` with a NULL `finished_at` forever.
    ///
    /// Reproduces: hold an occurrence at T1, then skip straight to evaluating
    /// T2 (as if no pass ran in between) once T2 is itself stale — the
    /// catch-up collapse advances `most_recent` to T2 without ever
    /// re-consulting the T1 row.
    #[tokio::test]
    async fn stranded_held_row_is_finalized_when_collapse_walks_past_it() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let t1 = utc_epoch(2026, 5, 28, 14, 0);
        let t2 = utc_epoch(2026, 5, 29, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, t1).unwrap();

        let dispatcher = FakeDispatcher::transient();
        let held = run_one_pass(&db, t1 + 5, &dispatcher).await;
        assert_eq!(held.held_transient, 1, "{held:?}");

        // Jump straight to well past T2 with no pass in between — the
        // catch-up collapse walks `most_recent` from T1 to T2 (both now
        // stale) in one tick.
        let pass = run_one_pass(&db, t2 + 20 * 60, &dispatcher).await;
        assert_eq!(pass.skipped_stale, 1, "{pass:?}");
        assert_eq!(pass.gave_up, 1, "the stranded T1 hold must be finalized: {pass:?}");

        let runs = db.list_automation_runs(&automation.id).unwrap();
        let t1_run = runs
            .iter()
            .find(|r| r.scheduled_for.parse::<i64>().unwrap() == t1)
            .expect("T1 run must still exist");
        assert_eq!(t1_run.outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP);
        assert!(t1_run.finished_at.is_some(), "T1 must reach a terminal state");
    }

    /// A gated dispatch (the global pause) writes no run row, advances nothing,
    /// and schedules no retry — only a human clearing the gate changes the
    /// answer, and doing so kicks the scheduler.
    #[tokio::test]
    async fn gated_dispatch_records_nothing_and_schedules_no_retry() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let dispatcher = FakeDispatcher::held();
        let pass = run_one_pass(&db, occ + 5, &dispatcher).await;

        assert_eq!(pass.held_gated, 1, "{pass:?}");
        assert_eq!(pass.held_transient, 0, "a pause is not a transient failure");
        assert_eq!(
            pass.wake_hint, None,
            "a gated hold must not ask the loop to wake — that is the 1 Hz spin",
        );
        assert!(
            db.list_automation_runs(&automation.id).unwrap().is_empty(),
            "a pause must not write an automation_runs row",
        );

        // The occurrence is untouched, so it is neither attempted nor aged.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(reloaded.next_due_at.unwrap().parse::<i64>().unwrap(), occ);
        assert_eq!(reloaded.last_outcome, None);
    }

    /// A gate consumes nothing. However many passes see the gate, the
    /// occurrence emerges unattempted and unaged, and the first pass after the
    /// gate clears fires *it* — not the next one. Contrast the old behaviour,
    /// where each gated pass wrote a `failed_will_retry` row and the 15-minute
    /// staleness clock ran the whole time, so the occurrence was guaranteed to
    /// be dropped before any human could clear the pause.
    ///
    /// (Passes still happen here only because `run_one_pass` is pause-agnostic
    /// by design — the loop itself stops calling it, which
    /// `paused_scheduler_sleeps_instead_of_spinning` covers. This pins the
    /// belt-and-braces path: a pause that lands mid-pass, and the manual
    /// `boss automation run` seam, both surface as `Held` too.)
    #[tokio::test]
    async fn gated_occurrence_is_not_consumed_and_fires_once_the_gate_clears() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let gated = FakeDispatcher::held();
        for offset in [5, 120, 300, 600] {
            run_one_pass(&db, occ + offset, &gated).await;
        }
        assert_eq!(gated.call_count(), 4, "every pass reached the gate");
        assert!(
            db.list_automation_runs(&automation.id).unwrap().is_empty(),
            "a gate must write no run rows, however many passes see it",
        );

        // Gate clears 11 minutes in — the occurrence is untouched, so the
        // catch-up window has not been consumed by the hold and it still fires.
        let resumed = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, occ + 11 * 60, &resumed).await;
        assert_eq!(pass.fired, 1, "{pass:?}");

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1, "exactly one row for the whole gated period");
        assert_eq!(
            runs[0].scheduled_for.parse::<i64>().unwrap(),
            occ,
            "the occurrence the gate held is the one that fires"
        );
    }

    /// A gate outlasting the catch-up window leaves the occurrence *stale*, not
    /// held: on resume it is skipped exactly once by the normal
    /// missed-while-down path and the schedule advances. One row for a pause of
    /// any length — the defect wrote one per second.
    #[tokio::test]
    async fn occurrence_gated_past_staleness_skips_once_on_resume() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        // A long pause: the loop runs no passes at all for its duration.
        // Resume lands a day later, with the occurrence long stale.
        let resumed = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, utc_epoch(2026, 5, 29, 13, 0), &resumed).await;
        assert_eq!(pass.skipped_stale, 1, "{pass:?}");
        assert_eq!(resumed.call_count(), 0);

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1, "exactly one row for the whole paused period");
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_SKIPPED);
        // Advanced to the next real occurrence, not the stale one.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 29, 14, 0)
        );
    }

    #[tokio::test]
    async fn suppressed_when_at_open_task_limit() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product); // limit 1
        create_open_task_for_automation(&db, &product, &automation.id);

        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();
        let now = occ + 5;

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.suppressed, 1, "{pass:?}");
        assert_eq!(dispatcher.call_count(), 0, "must not dispatch while at cap");

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT);
        // Advanced past the suppressed occurrence.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 29, 14, 0)
        );
    }

    #[tokio::test]
    async fn stale_occurrence_skipped_and_advanced() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        // next_due was 2 days ago; now is just before today's 2pm. The
        // most-recent occurrence (yesterday 2pm) is >24h stale.
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 26, 14, 0))
            .unwrap();
        let now = utc_epoch(2026, 5, 28, 13, 0);

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.skipped_stale, 1, "{pass:?}");
        assert_eq!(dispatcher.call_count(), 0);

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_SKIPPED);
        // Advanced to today's 2pm (the next future occurrence).
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 28, 14, 0)
        );
    }

    #[tokio::test]
    async fn slightly_late_wake_catches_up_within_window() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();
        // Woke 10 minutes late — within the 15-minute window → fire (catch up).
        let now = occ + 10 * 60;

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.fired, 1, "{pass:?}");
        assert_eq!(
            db.list_automation_runs(&automation.id).unwrap()[0]
                .scheduled_for
                .parse::<i64>()
                .unwrap(),
            occ,
            "must fire the missed occurrence, not skip it"
        );
    }

    #[tokio::test]
    async fn high_frequency_outage_collapses_to_most_recent() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = db
            .create_automation(
                CreateAutomationInput::builder()
                    .product_id(product.clone())
                    .name("every-5-min")
                    .trigger(AutomationTrigger::Schedule {
                        cron: "*/5 * * * *".to_owned(),
                        timezone: "UTC".to_owned(),
                    })
                    .standing_instruction("x")
                    .build(),
            )
            .unwrap();
        // next_due 14:00; asleep until 14:32. Occurrences 14:00..14:30 missed.
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 28, 14, 0))
            .unwrap();
        let now = utc_epoch(2026, 5, 28, 14, 32);

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        // Fires exactly once, for 14:30 (most recent within window), not 7x.
        assert_eq!(pass.fired, 1, "{pass:?}");
        assert_eq!(dispatcher.call_count(), 1);
        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].scheduled_for.parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 28, 14, 30)
        );
        // Advanced to 14:35.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 28, 14, 35)
        );
    }

    #[tokio::test]
    async fn disabled_automation_is_not_evaluated() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 28, 14, 0))
            .unwrap();
        db.disable_automation(&automation.id).unwrap();

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, utc_epoch(2026, 5, 28, 15, 0), &dispatcher).await;
        assert_eq!(pass.evaluated, 0);
        assert_eq!(dispatcher.call_count(), 0);
    }

    #[tokio::test]
    async fn not_due_automation_does_nothing() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 28, 14, 0))
            .unwrap();
        // now is before next_due → list_due_automations must not return it.
        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, utc_epoch(2026, 5, 28, 13, 0), &dispatcher).await;
        assert_eq!(pass.evaluated, 0);
    }

    /// End-to-end of the math + scheduler: park next_due via the same
    /// occurrence function the scheduler uses, fire, and confirm the advance
    /// matches the next computed occurrence.
    #[tokio::test]
    async fn advance_matches_computed_next_occurrence() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let dispatcher = FakeDispatcher::dispatched();
        run_one_pass(&db, occ + 1, &dispatcher).await;

        let expected_following = next_occurrence_after_str("0 14 * * *", "UTC", occ).unwrap().unwrap();
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            expected_following
        );
    }

    /// next_sleep_secs targets the earliest automation's next_due_at, not a
    /// fixed coarse interval.
    #[tokio::test]
    async fn min_next_fire_sleep_targets_earliest_automation() {
        let (_d, db) = open_db();
        let product = create_product(&db);

        // Two automations: one due at 2pm, one at 3pm.
        let early = create_daily_automation(&db, &product);
        let late = db
            .create_automation(
                CreateAutomationInput::builder()
                    .product_id(product.clone())
                    .name("3pm")
                    .trigger(AutomationTrigger::Schedule {
                        cron: "0 15 * * *".to_owned(),
                        timezone: "UTC".to_owned(),
                    })
                    .standing_instruction("x")
                    .build(),
            )
            .unwrap();

        let t2pm = utc_epoch(2026, 5, 28, 14, 0);
        let t3pm = utc_epoch(2026, 5, 28, 15, 0);
        db.initialize_automation_next_due_at(&early.id, t2pm).unwrap();
        db.initialize_automation_next_due_at(&late.id, t3pm).unwrap();

        // At 1pm, sleep should target 2pm (earliest).
        let t1pm = utc_epoch(2026, 5, 28, 13, 0);
        let sleep = next_sleep_secs(&db, t1pm, false);
        assert_eq!(sleep as i64, t2pm - t1pm, "should sleep exactly until 2pm");

        // After advancing early's next_due to tomorrow 2pm (simulating it fired),
        // sleep should target today 3pm.
        db.initialize_automation_next_due_at(&early.id, utc_epoch(2026, 5, 29, 14, 0))
            .unwrap();
        let t2pm_plus_5 = t2pm + 5;
        let sleep2 = next_sleep_secs(&db, t2pm_plus_5, false);
        assert_eq!(sleep2 as i64, t3pm - t2pm_plus_5, "should target 3pm after 2pm fires");
    }

    /// With no enabled automations, next_sleep_secs falls back to the maximum.
    #[test]
    fn no_automations_sleep_is_max() {
        let (_d, db) = open_db();
        let sleep = next_sleep_secs(&db, utc_epoch(2026, 5, 28, 14, 0), false);
        assert_eq!(sleep, AUTOMATION_SCHEDULER_MAX_SLEEP_SECS);
    }

    /// Uninitialized automation (next_due_at IS NULL) triggers the short poll.
    #[tokio::test]
    async fn uninitialized_automation_uses_short_poll() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        // Created but never seen by a scheduler pass → next_due_at IS NULL.
        let _automation = create_daily_automation(&db, &product);
        let sleep = next_sleep_secs(&db, utc_epoch(2026, 5, 28, 14, 0), false);
        assert_eq!(sleep, AUTOMATION_SCHEDULER_UNINITIALIZED_POLL_SECS);
    }

    /// The other half of the pause fix. Skipping the pass is not enough on its
    /// own: while paused nothing ever advances `next_due_at`, so every due
    /// occurrence sits permanently in the past. If the sleep still floored a
    /// past-due occurrence at one second, the loop would wake, re-query and
    /// re-sleep every second for the whole pause — which is exactly what it
    /// did (~0.96 passes/s measured over a 5,182 s window).
    #[tokio::test]
    async fn paused_scheduler_sleeps_instead_of_spinning() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        // An hour past due and never advanced — the paused steady state.
        let now = occ + 3600;
        assert_eq!(next_sleep_secs(&db, now, true), AUTOMATION_SCHEDULER_MAX_SLEEP_SECS);

        // Even an uninitialized automation, whose short poll normally wins,
        // must not wake a paused scheduler: initialising it would be pointless
        // work that a resume kick will do immediately anyway.
        let _fresh = create_daily_automation(&db, &product);
        assert_eq!(next_sleep_secs(&db, now, true), AUTOMATION_SCHEDULER_MAX_SLEEP_SECS);
    }

    /// A past-due occurrence the pass could not act on (held for retry, or a
    /// broken cron) must not floor the sleep at one second. The pass paces
    /// those cases through `wake_hint` instead.
    #[tokio::test]
    async fn past_due_occurrence_does_not_floor_the_sleep() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let dispatcher = FakeDispatcher::transient();
        let pass = run_one_pass(&db, occ + 5, &dispatcher).await;

        // The occurrence is held in the past...
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(reloaded.next_due_at.unwrap().parse::<i64>().unwrap(), occ);
        // ...and no longer drags the sleep down to 1s.
        assert_eq!(
            next_sleep_secs(&db, occ + 5, false),
            AUTOMATION_SCHEDULER_MAX_SLEEP_SECS,
        );
        // The retry is paced explicitly by the pass instead.
        assert_eq!(pass.wake_hint, Some(occ + 5 + AUTOMATION_RETRY_INTERVAL_SECS));
    }

    /// The regression the sleep change introduces, pinned end-to-end against a
    /// real `spawn_loop`.
    ///
    /// A paused scheduler now sleeps up to `AUTOMATION_SCHEDULER_MAX_SLEEP_SECS`
    /// (one hour) instead of spinning, which means resume is only prompt if the
    /// pause handler kicks it. `handle_set_automation_paused` does — and if that
    /// notify is ever dropped, this test hangs on the hour-long sleep until the
    /// timeout fires rather than passing quietly.
    #[tokio::test]
    async fn resume_kick_wakes_a_paused_scheduler_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (_d, db) = open_db_arc();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        // Park the occurrence just behind real wall-clock now, so any pass that
        // runs fires it immediately. `spawn_loop` reads the real clock.
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        db.initialize_automation_next_due_at(&automation.id, now - 5).unwrap();

        let paused = Arc::new(AtomicBool::new(true));
        let kick = Arc::new(Notify::new());
        let dispatcher = Arc::new(FakeDispatcher::dispatched());
        let paused_for_loop = paused.clone();
        let _handle = spawn_loop(
            db.clone(),
            dispatcher.clone(),
            kick.clone(),
            Arc::new(move || paused_for_loop.load(Ordering::Acquire)),
        );

        // While paused the loop must evaluate nothing, however long it sits.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(dispatcher.call_count(), 0, "a paused scheduler must not dispatch");
        assert!(
            db.list_automation_runs(&automation.id).unwrap().is_empty(),
            "a paused scheduler must not write automation_runs rows"
        );

        // Resume, exactly as `handle_set_automation_paused` does: clear the
        // flag, then kick.
        paused.store(false, Ordering::Release);
        kick.notify_one();

        // Without the kick this would wait out the full max sleep.
        let fired = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if dispatcher.call_count() > 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            fired.is_ok(),
            "resume must take effect promptly; the scheduler slept through the kick",
        );
    }

    /// A held occurrence must not delay an unrelated automation that is
    /// genuinely due sooner: the loop takes the earlier of the sleep target and
    /// the pass's wake hint.
    #[tokio::test]
    async fn held_occurrence_does_not_delay_another_automation() {
        let (_d, db) = open_db();
        let product = create_product(&db);

        let held = create_daily_automation(&db, &product);
        let other = db
            .create_automation(
                CreateAutomationInput::builder()
                    .product_id(product.clone())
                    .name("3pm")
                    .trigger(AutomationTrigger::Schedule {
                        cron: "0 15 * * *".to_owned(),
                        timezone: "UTC".to_owned(),
                    })
                    .standing_instruction("x")
                    .build(),
            )
            .unwrap();

        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&held.id, occ).unwrap();
        // The other automation is due 30s from now — sooner than the retry interval.
        let now = occ + 5;
        db.initialize_automation_next_due_at(&other.id, now + 30).unwrap();

        let dispatcher = FakeDispatcher::transient();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        let base = next_sleep_secs(&db, now, false);
        let wake = pass.wake_hint.expect("held occurrence schedules a retry");
        let effective = base.min((wake - now).clamp(1, AUTOMATION_SCHEDULER_MAX_SLEEP_SECS as i64) as u64);
        assert_eq!(effective, 30, "must wake for the sooner automation, not the retry");
    }

    /// Two automations with distinct cron-minute fields each fire on their
    /// correct minute; the scheduler does not wake between them (verified by
    /// run_one_pass returning evaluated=0 between the two fire times).
    #[tokio::test]
    async fn two_automations_each_fire_on_correct_cron_minute() {
        let (_d, db) = open_db();
        let product = create_product(&db);

        // A fires at minute 21, B fires at minute 45, both in UTC.
        let auto_a = db
            .create_automation(
                CreateAutomationInput::builder()
                    .product_id(product.clone())
                    .name("minute-21")
                    .trigger(AutomationTrigger::Schedule {
                        cron: "21 * * * *".to_owned(),
                        timezone: "UTC".to_owned(),
                    })
                    .standing_instruction("x")
                    .build(),
            )
            .unwrap();
        let auto_b = db
            .create_automation(
                CreateAutomationInput::builder()
                    .product_id(product.clone())
                    .name("minute-45")
                    .trigger(AutomationTrigger::Schedule {
                        cron: "45 * * * *".to_owned(),
                        timezone: "UTC".to_owned(),
                    })
                    .standing_instruction("y")
                    .build(),
            )
            .unwrap();

        // Initialise at 23:00; both automations pick up their correct minutes.
        let t_23_00 = utc_epoch(2026, 5, 28, 23, 0);
        let dispatcher = FakeDispatcher::dispatched();
        let init_pass = run_one_pass(&db, t_23_00, &dispatcher).await;
        assert_eq!(init_pass.initialized, 2);
        assert_eq!(init_pass.fired, 0);

        let a_next: i64 = db
            .get_automation(&auto_a.id)
            .unwrap()
            .unwrap()
            .next_due_at
            .unwrap()
            .parse()
            .unwrap();
        let b_next: i64 = db
            .get_automation(&auto_b.id)
            .unwrap()
            .unwrap()
            .next_due_at
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(a_next, utc_epoch(2026, 5, 28, 23, 21), "A must target minute 21");
        assert_eq!(b_next, utc_epoch(2026, 5, 28, 23, 45), "B must target minute 45");

        // Sleep should target A (earliest).
        let sleep_after_init = next_sleep_secs(&db, t_23_00, false);
        assert_eq!(sleep_after_init as i64, a_next - t_23_00);

        // No evaluation between 23:00 and 23:21.
        let t_23_10 = utc_epoch(2026, 5, 28, 23, 10);
        let pass_between = run_one_pass(&db, t_23_10, &dispatcher).await;
        assert_eq!(pass_between.evaluated, 0, "no automation due at 23:10");

        // A fires at 23:21+5s; B must not fire yet.
        let t_a_fire = a_next + 5;
        let pass_a = run_one_pass(&db, t_a_fire, &dispatcher).await;
        assert_eq!(pass_a.fired, 1, "only A should fire at 23:21");
        assert_eq!(pass_a.evaluated, 1, "only A in the due list");
        let a_calls: Vec<_> = dispatcher.calls.lock().unwrap().clone();
        assert_eq!(a_calls.len(), 1);
        assert_eq!(a_calls[0].1, a_next, "A must be dispatched for its cron minute");

        // After A fires, sleep targets B.
        let sleep_after_a = next_sleep_secs(&db, t_a_fire, false);
        assert_eq!(sleep_after_a as i64, b_next - t_a_fire);

        // B fires at 23:45+5s.
        let t_b_fire = b_next + 5;
        let pass_b = run_one_pass(&db, t_b_fire, &dispatcher).await;
        assert_eq!(pass_b.fired, 1, "B fires at 23:45");
        assert_eq!(pass_b.evaluated, 1, "only B in the due list");
        let all_calls = dispatcher.calls.lock().unwrap().clone();
        assert_eq!(all_calls[1].1, b_next, "B dispatched for its cron minute");
    }

    /// Updating a trigger resets next_due_at so the scheduler recomputes from
    /// the new cron expression on the next pass (fixes the stale-schedule bug).
    #[test]
    fn update_trigger_resets_next_due_at() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product); // 0 14 * * *
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 28, 14, 0))
            .unwrap();

        // Confirm it is set.
        let before = db.get_automation(&automation.id).unwrap().unwrap();
        assert!(before.next_due_at.is_some());

        // Change the trigger to a different cron.
        db.update_automation(
            &automation.id,
            AutomationPatch {
                trigger: Some(AutomationTrigger::Schedule {
                    cron: "21 * * * *".to_owned(),
                    timezone: "UTC".to_owned(),
                }),
                ..Default::default()
            },
        )
        .unwrap();

        // next_due_at must be NULL so the scheduler initialises from the new cron.
        let after = db.get_automation(&automation.id).unwrap().unwrap();
        assert!(
            after.next_due_at.is_none(),
            "next_due_at must be reset to NULL after trigger update"
        );
    }

    /// Updating fields other than the trigger must NOT reset next_due_at.
    #[test]
    fn update_non_trigger_fields_preserve_next_due_at() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let due = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, due).unwrap();

        db.update_automation(
            &automation.id,
            AutomationPatch {
                name: Some("renamed".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let after = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            after.next_due_at.unwrap().parse::<i64>().unwrap(),
            due,
            "non-trigger update must not touch next_due_at"
        );
    }
}
