//! Runtime envelope watchdog — the safety net beneath the planner
//! decomposition gate and the design-brief sizing contract.
//!
//! ## What it guards against
//!
//! T298 ("Full national rolling-points PDF detail parse") ran well over an
//! hour in a single worker session — a multi-table parse plus a seed path
//! plus an all-lists validation sweep, shipped as one task. Two upstream
//! guardrails now push breakdowns to arrive pre-split (the design-worker
//! sizing contract) and reject monolithic proposals at plan time (the planner
//! decomposition gate). This watchdog catches the briefs that slip past both:
//! a *live* execution whose wall-clock has already blown past the envelope its
//! effort class implies is very likely an under-decomposed row.
//!
//! ## Signal only — never interrupt
//!
//! Unlike [`crate::stale_worker_sweep`], which reaps a wedged worker, this
//! sweep does **not** touch the worker: no reap, no orphan, no lease release.
//! An overrun is detection, not enforcement — the worker keeps running and an
//! operator-visible attention item is filed so a human can decide whether the
//! row should have been split. A legitimately long `large` run is expected to
//! trip this; the signal is a prompt to reconsider decomposition, not a kill.
//!
//! ## Exactly one signal per execution
//!
//! The sweep runs every 60s. To avoid piling up duplicate items, it files the
//! `envelope_overrun` attention item at most once per execution — it checks
//! for an existing open item keyed to the execution before filing. Durable
//! (DB-backed), so it survives an engine restart mid-run.
//!
//! ## Calibration ground truth
//!
//! Every pass an execution is observed over its envelope, the sweep also emits
//! a structured `envelope-calibration` log line (actual duration vs the row's
//! effort class) — the "simple audit log" the brief asks for, so the
//! effort-heuristic and planner-sizing thresholds can be tuned against
//! observed reality. (Per-execution *token* accounting has no source in the
//! worker-session pipeline today; duration is the cost signal T298 showed
//! dominates — ~42% of that run was model thinking time, not builds.)

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use boss_event_bus::{Event, EventBus, EventKind, TopicFilter, spawn_supervised};
use boss_protocol::{CreateAttentionItemInput, EffortLevel, LiveWorkerState, WorkItem, WorkerActivity};
use boss_timer_wheel::TimerWheel;

use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// Default per-effort duration envelopes (seconds). Calibrated for opus/xhigh
/// thinking time on the larger rows — the T298 analysis showed model
/// thinking, not builds, dominates wall-clock, so these are generous. An
/// execution still under its envelope is healthy; over it is the signal.
///
/// `Max` deliberately has no envelope: it is the human-only "maximum
/// reasoning depth regardless of scope" override, so a long run there is
/// expected and must not signal.
pub const DEFAULT_ENVELOPE_TRIVIAL_SECS: i64 = 10 * 60;
pub const DEFAULT_ENVELOPE_SMALL_SECS: i64 = 15 * 60;
pub const DEFAULT_ENVELOPE_MEDIUM_SECS: i64 = 30 * 60;
pub const DEFAULT_ENVELOPE_LARGE_SECS: i64 = 60 * 60;

/// `kind` of the attention item this sweep files on an overrun. One kind
/// keeps the surface simple; the overrun specifics live in the title/body.
pub const ENVELOPE_OVERRUN_ATTENTION_KIND: &str = "envelope_overrun";

/// Per-effort duration envelopes, in seconds. A per-class threshold of `<= 0`
/// disables the envelope for that class (never signal). Configurable via
/// `BOSS_ENVELOPE_{TRIVIAL,SMALL,MEDIUM,LARGE}_SECS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvelopeThresholds {
    pub trivial: i64,
    pub small: i64,
    pub medium: i64,
    pub large: i64,
}

impl Default for EnvelopeThresholds {
    fn default() -> Self {
        Self {
            trivial: DEFAULT_ENVELOPE_TRIVIAL_SECS,
            small: DEFAULT_ENVELOPE_SMALL_SECS,
            medium: DEFAULT_ENVELOPE_MEDIUM_SECS,
            large: DEFAULT_ENVELOPE_LARGE_SECS,
        }
    }
}

impl EnvelopeThresholds {
    /// Load the envelopes from an injected env lookup, falling back to the
    /// defaults for any var that is unset or unparseable. Tests call this with
    /// an explicit closure so they never touch the process environment.
    pub fn from_env(lookup: impl Fn(&str) -> Option<OsString>) -> Self {
        let default = Self::default();
        let get = |name: &str, fallback: i64| -> i64 {
            match lookup(name) {
                None => fallback,
                Some(raw) => match raw.to_string_lossy().trim().parse::<i64>() {
                    Ok(v) => v,
                    Err(_) => {
                        tracing::warn!(name, value = %raw.to_string_lossy(), "envelope-watch: unparseable env override; using default");
                        fallback
                    }
                },
            }
        };
        Self {
            trivial: get("BOSS_ENVELOPE_TRIVIAL_SECS", default.trivial),
            small: get("BOSS_ENVELOPE_SMALL_SECS", default.small),
            medium: get("BOSS_ENVELOPE_MEDIUM_SECS", default.medium),
            large: get("BOSS_ENVELOPE_LARGE_SECS", default.large),
        }
    }

    /// The envelope (seconds) for an effort class, or `None` when the class
    /// has no envelope: `Max` (human override) always, and any class whose
    /// configured threshold is `<= 0` (disabled).
    pub fn for_effort(&self, effort: EffortLevel) -> Option<i64> {
        let secs = match effort {
            EffortLevel::Trivial => self.trivial,
            EffortLevel::Small => self.small,
            EffortLevel::Medium => self.medium,
            EffortLevel::Large => self.large,
            EffortLevel::Max => return None,
        };
        (secs > 0).then_some(secs)
    }
}

/// Counts from one sweep pass; logged at `info` when a new overrun is filed.
#[derive(Debug, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct EnvelopeSweepOutcome {
    /// New overrun attention items filed this pass.
    pub signaled: usize,
    /// Executions observed over their envelope (includes already-signaled).
    pub over_envelope: usize,
    /// Executions checked and found within their envelope.
    pub within_envelope: usize,
    /// Over-envelope executions that already carried an open overrun item.
    pub already_signaled: usize,
    /// Slots skipped because they were not actively `Working`.
    pub not_working_skipped: usize,
    /// Executions skipped because their effort class has no envelope
    /// (`Max`, disabled, or the row is unclassified).
    pub no_envelope_skipped: usize,
    /// Slots skipped because the execution had no parseable `started_at`.
    pub no_started_at_skipped: usize,
}

impl crate::sweep_loop::SweepOutcome for EnvelopeSweepOutcome {
    fn has_activity(&self) -> bool {
        self.signaled > 0
    }

    fn log(&self) {
        tracing::info!(
            signaled = self.signaled,
            over_envelope = self.over_envelope,
            already_signaled = self.already_signaled,
            within_envelope = self.within_envelope,
            "envelope-watch: filed overrun signal(s)",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`, firing
/// once immediately on boot (so a run that overran before an engine restart is
/// re-signalled at boot without waiting for the first interval).
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    thresholds: EnvelopeThresholds,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let live_states = Arc::clone(&live_states);
        async move {
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            run_one_pass(work_db.as_ref(), live_states.as_ref(), &thresholds, now)
        }
    })
}

/// Run a single envelope sweep pass. Pure detection + best-effort signal: it
/// reads live worker state and the DB, files an attention item on a fresh
/// overrun, and returns a summary. `now_epoch_secs` is injected for
/// deterministic tests.
pub fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    thresholds: &EnvelopeThresholds,
    now_epoch_secs: i64,
) -> EnvelopeSweepOutcome {
    let mut outcome = EnvelopeSweepOutcome::default();

    for state in live_states.snapshot() {
        match evaluate_slot(work_db, thresholds, now_epoch_secs, &state) {
            SlotOutcome::NotWorking => outcome.not_working_skipped += 1,
            SlotOutcome::LookupFailed | SlotOutcome::Terminal => {}
            SlotOutcome::NoStartedAt => outcome.no_started_at_skipped += 1,
            SlotOutcome::NoEnvelope => outcome.no_envelope_skipped += 1,
            SlotOutcome::WithinEnvelope => outcome.within_envelope += 1,
            SlotOutcome::AlreadySignaled => {
                outcome.over_envelope += 1;
                outcome.already_signaled += 1;
            }
            SlotOutcome::Signaled => {
                outcome.over_envelope += 1;
                outcome.signaled += 1;
            }
        }
    }

    outcome
}

/// Result of evaluating one live slot against its envelope. Shared by the
/// periodic sweep ([`run_one_pass`]) and the timer-triggered single-slot
/// recheck ([`handle_timer_deadline`]) so both paths file the exact same
/// overrun signal through the exact same idempotency guard.
enum SlotOutcome {
    /// Slot isn't actively `Working` — not a candidate.
    NotWorking,
    /// The execution row couldn't be looked up (already logged a `warn`).
    LookupFailed,
    /// A completion raced the check — the execution is already terminal.
    Terminal,
    /// The execution has no parseable `started_at` yet.
    NoStartedAt,
    /// The work item's effort class has no envelope (unclassified, `Max`,
    /// or disabled via env).
    NoEnvelope,
    /// Elapsed time is within the effort class's envelope.
    WithinEnvelope,
    /// Over envelope, but an open overrun item already exists.
    AlreadySignaled,
    /// Over envelope; a fresh overrun attention item was filed.
    Signaled,
}

/// Evaluate a single live slot against its envelope, filing the overrun
/// attention item (via [`file_overrun_attention`]) exactly once per
/// execution. Pure detection plus the same best-effort signal side effect
/// `run_one_pass` always had — extracted so the timer subscriber can run the
/// identical check for one execution without re-scanning every live slot.
fn evaluate_slot(
    work_db: &WorkDb,
    thresholds: &EnvelopeThresholds,
    now_epoch_secs: i64,
    state: &LiveWorkerState,
) -> SlotOutcome {
    // Only actively-working slots count toward the work-time envelope. A
    // slot waiting for human input or idle is blocked on the operator,
    // not overrunning on compute; `Spawning` has no `started_at` yet. A
    // long foreground build keeps the slot `Working`, and that time
    // legitimately counts against the envelope — so, unlike the stale
    // sweep, we do NOT skip a tool-in-flight slot here.
    if state.activity != WorkerActivity::Working {
        return SlotOutcome::NotWorking;
    }

    let execution_id = &state.run_id;
    let Some(execution) = crate::sweep_loop::lookup_execution_or_warn(
        work_db,
        execution_id,
        "envelope-watch: failed to look up execution; skipping slot",
    ) else {
        return SlotOutcome::LookupFailed;
    };

    // A completion may have raced the sweep — a terminal execution is done.
    if execution.status.is_terminal() {
        return SlotOutcome::Terminal;
    }

    let Some(started_epoch) = execution.started_epoch() else {
        return SlotOutcome::NoStartedAt;
    };
    let elapsed_secs = (now_epoch_secs - started_epoch).max(0);

    // Resolve the row's effort class → its envelope. An unclassified row,
    // `Max`, or a disabled class has no envelope and is never signalled.
    let Some(effort) = effort_for_work_item(work_db, &execution.work_item_id) else {
        return SlotOutcome::NoEnvelope;
    };
    let Some(envelope_secs) = thresholds.for_effort(effort) else {
        return SlotOutcome::NoEnvelope;
    };

    if elapsed_secs <= envelope_secs {
        return SlotOutcome::WithinEnvelope;
    }

    // Calibration ground truth (audit log): actual duration vs the row's
    // effort class, so envelope/effort heuristics can be tuned against
    // reality. Cheap and queryable; emitted every pass an execution is over.
    tracing::info!(
        marker = "envelope-calibration",
        execution_id = %execution_id,
        work_item_id = %execution.work_item_id,
        effort = effort.as_str(),
        elapsed_secs,
        envelope_secs,
        over_by_secs = elapsed_secs - envelope_secs,
        "envelope-watch: execution over its effort-class envelope",
    );

    // Signal only, exactly once per execution. Skip if an open overrun
    // item already exists for this execution.
    if execution_already_signaled(work_db, execution_id) {
        return SlotOutcome::AlreadySignaled;
    }

    file_overrun_attention(
        work_db,
        execution_id,
        &execution.work_item_id,
        effort,
        elapsed_secs,
        envelope_secs,
    );
    SlotOutcome::Signaled
}

/// Subscribe to the timer-wheel's `Event::Timer` topic for a fast per-
/// execution envelope recheck, racing the 60s sweep [`spawn_loop`] keeps
/// running as the untouched backstop.
///
/// Each attempt (initial start, or restart after a panic — see
/// [`spawn_supervised`]) schedules one deadline per currently-live `Working`
/// slot that has an envelope, then processes `Timer` events forever: a
/// `Timer{deadline_id}` whose id names a still-live slot triggers exactly the
/// same [`evaluate_slot`] check `run_one_pass` runs for that slot, so the two
/// paths share one idempotency guard ([`execution_already_signaled`]). A slot
/// that starts working after this attempt's initial scheduling pass has no
/// timer-wheel deadline until the next restart — the sweep is what catches
/// it in the meantime, exactly as the design doc's "the bus never replaces a
/// sweep; it races it" invariant expects.
pub fn spawn_timer_subscriber(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    timer_wheel: Arc<TimerWheel>,
    event_bus: Arc<EventBus>,
    thresholds: EnvelopeThresholds,
) -> tokio::task::JoinHandle<()> {
    spawn_supervised("envelope_watch_timer", move || {
        let work_db = Arc::clone(&work_db);
        let live_states = Arc::clone(&live_states);
        let timer_wheel = Arc::clone(&timer_wheel);
        let event_bus = Arc::clone(&event_bus);
        async move {
            let mut subscription = event_bus.subscribe(TopicFilter::kind(EventKind::Timer));
            schedule_envelope_deadlines(&work_db, &live_states, &timer_wheel, &thresholds);
            while let Some(event) = subscription.recv().await {
                let Event::Timer { deadline_id } = event else {
                    continue;
                };
                handle_timer_deadline(&work_db, &live_states, &thresholds, &deadline_id);
            }
        }
    })
}

/// Schedule a timer-wheel deadline — keyed by execution id — for every
/// currently-live `Working` slot whose work item has an envelope, timed to
/// elapse when that execution's envelope does. Slots with no envelope, no
/// `started_at`, or that are already terminal are skipped exactly like
/// [`evaluate_slot`] would skip them; there's nothing to schedule for a slot
/// that will never signal.
fn schedule_envelope_deadlines(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    timer_wheel: &TimerWheel,
    thresholds: &EnvelopeThresholds,
) {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    for state in live_states.snapshot() {
        if state.activity != WorkerActivity::Working {
            continue;
        }
        let execution_id = state.run_id.clone();
        let Ok(execution) = work_db.get_execution(&execution_id) else {
            continue;
        };
        if execution.status.is_terminal() {
            continue;
        }
        let Some(started_epoch) = execution.started_epoch() else {
            continue;
        };
        let Some(effort) = effort_for_work_item(work_db, &execution.work_item_id) else {
            continue;
        };
        let Some(envelope_secs) = thresholds.for_effort(effort) else {
            continue;
        };
        let elapsed_secs = (now - started_epoch).max(0);
        let remaining_secs = (envelope_secs - elapsed_secs).max(0) as u64;
        timer_wheel.schedule_after(execution_id, Duration::from_secs(remaining_secs));
    }
}

/// Handle one `Timer{deadline_id}` event from [`schedule_envelope_deadlines`]:
/// `deadline_id` is an execution id, so look up that slot's current live
/// state (it may have gone terminal or been released since scheduling — in
/// which case there's nothing to check) and run the identical single-slot
/// envelope evaluation the sweep runs.
fn handle_timer_deadline(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    thresholds: &EnvelopeThresholds,
    execution_id: &str,
) {
    let Some(state) = live_states.snapshot().into_iter().find(|s| s.run_id == execution_id) else {
        return;
    };
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    evaluate_slot(work_db, thresholds, now, &state);
}

/// The effort classification of the work item behind an execution, if any.
/// `None` for an unclassified row or a non-task/chore work item.
fn effort_for_work_item(work_db: &WorkDb, work_item_id: &str) -> Option<EffortLevel> {
    match work_db.get_work_item(work_item_id) {
        Ok(WorkItem::Task(t) | WorkItem::Chore(t)) => t.effort_level,
        _ => None,
    }
}

/// Whether an open `envelope_overrun` attention item already exists for this
/// execution — the per-execution idempotency guard. On a lookup error we
/// assume "already signalled" so a transient failure never double-files.
fn execution_already_signaled(work_db: &WorkDb, execution_id: &str) -> bool {
    match work_db.list_attention_items(execution_id) {
        Ok(items) => items
            .iter()
            .any(|item| item.kind == ENVELOPE_OVERRUN_ATTENTION_KIND && item.status == "open"),
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "envelope-watch: attention lookup failed; skipping signal this pass"
            );
            true
        }
    }
}

/// File the operator-visible overrun attention item. Best-effort: a failure to
/// surface must not fail the pass (there is nothing to roll back — this is a
/// courtesy signal).
fn file_overrun_attention(
    work_db: &WorkDb,
    execution_id: &str,
    work_item_id: &str,
    effort: EffortLevel,
    elapsed_secs: i64,
    envelope_secs: i64,
) {
    let over_by_secs = elapsed_secs - envelope_secs;
    let title = format!("Worker over its `{}` effort envelope", effort.as_str());
    let body = format!(
        "This execution has been running for {} min — past the ~{} min envelope for its `{}` effort \
         classification (over by {} min).\n\n\
         This is an informational signal, not an interruption: the worker keeps running. A run that \
         overruns its effort-class envelope is often a sign the row was **under-decomposed** — it packed \
         more than one reviewable-PR-per-session unit of work. Consider whether it should have been split \
         into dependency-ordered tasks; if the classification was simply too low, bump its effort.\n\n\
         - work item: `{}`\n\
         - execution: `{}`\n\
         - effort: `{}`\n\
         - elapsed: {}s (envelope {}s, over by {}s)\n",
        elapsed_secs / 60,
        envelope_secs / 60,
        effort.as_str(),
        over_by_secs / 60,
        work_item_id,
        execution_id,
        effort.as_str(),
        elapsed_secs,
        envelope_secs,
        over_by_secs,
    );

    if let Err(err) = work_db.create_attention_item(
        CreateAttentionItemInput::builder()
            .kind(ENVELOPE_OVERRUN_ATTENTION_KIND)
            .title(title)
            .body_markdown(body)
            .execution_id(execution_id.to_owned())
            .status("open")
            .build(),
    ) {
        tracing::warn!(
            execution_id,
            ?err,
            "envelope-watch: failed to file overrun attention item (non-fatal)"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use boss_protocol::{WorkItemBinding, WorkItemPatch, WorkerEvent};

    use super::*;
    use crate::test_support::{create_active_chore, create_execution_started_secs_ago, create_product, open_db};
    use crate::work::WorkDb;

    fn now() -> i64 {
        boss_engine_utils::epoch_time::now_epoch_secs()
    }

    /// Set the work item's effort classification.
    fn set_effort(db: &WorkDb, work_item_id: &str, effort: EffortLevel) {
        db.update_work_item(
            work_item_id,
            WorkItemPatch {
                effort_level: Some(effort.as_str().to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
    }

    /// Register a live slot bound to `execution_id`/`work_item_id` and drive
    /// it to `Working` (a balanced PreToolUse/PostToolUse pair).
    fn register_working_slot(
        live_states: &LiveWorkerStateRegistry,
        slot_id: u8,
        execution_id: &str,
        work_item_id: &str,
    ) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-8",
            std::process::id() as i32,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
        live_states.apply_event(
            slot_id,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        live_states.apply_event(
            slot_id,
            &WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );
    }

    fn overrun_items(db: &WorkDb, execution_id: &str) -> usize {
        db.list_attention_items(execution_id)
            .unwrap()
            .into_iter()
            .filter(|i| i.kind == ENVELOPE_OVERRUN_ATTENTION_KIND && i.status == "open")
            .count()
    }

    /// The core acceptance invariant: a worker past its effort-class envelope
    /// produces exactly one visible overrun signal, and re-running the sweep
    /// never files a second.
    #[test]
    fn over_envelope_execution_is_signaled_exactly_once() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "oversize chore");
        set_effort(&db, &work_item_id, EffortLevel::Small); // 15 min envelope
        // Started 20 minutes ago — comfortably past the 15-minute small envelope.
        let execution_id = create_execution_started_secs_ago(&db, &work_item_id, 20 * 60);

        let live_states = LiveWorkerStateRegistry::new();
        register_working_slot(&live_states, 1, &execution_id, &work_item_id);

        let thresholds = EnvelopeThresholds::default();
        let outcome = run_one_pass(&db, &live_states, &thresholds, now());
        assert_eq!(outcome.signaled, 1, "an over-envelope worker must be signalled");
        assert_eq!(outcome.over_envelope, 1);
        assert_eq!(overrun_items(&db, &execution_id), 1, "exactly one overrun item");

        // A second sweep must NOT file a duplicate — exactly one per execution.
        let outcome2 = run_one_pass(&db, &live_states, &thresholds, now());
        assert_eq!(outcome2.signaled, 0, "no duplicate signal on the next pass");
        assert_eq!(outcome2.already_signaled, 1);
        assert_eq!(overrun_items(&db, &execution_id), 1, "still exactly one overrun item");
    }

    /// A worker still within its envelope is left alone (the common case).
    #[test]
    fn within_envelope_execution_is_not_signaled() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "healthy chore");
        set_effort(&db, &work_item_id, EffortLevel::Small); // 15 min envelope
        let execution_id = create_execution_started_secs_ago(&db, &work_item_id, 60); // 1 min in

        let live_states = LiveWorkerStateRegistry::new();
        register_working_slot(&live_states, 1, &execution_id, &work_item_id);

        let outcome = run_one_pass(&db, &live_states, &EnvelopeThresholds::default(), now());
        assert_eq!(outcome.signaled, 0);
        assert_eq!(outcome.within_envelope, 1);
        assert_eq!(overrun_items(&db, &execution_id), 0);
    }

    /// An unclassified row has no envelope, so a long run never signals.
    #[test]
    fn unclassified_effort_has_no_envelope() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "unclassified chore");
        // No set_effort — effort_level stays None.
        let execution_id = create_execution_started_secs_ago(&db, &work_item_id, 5 * 60 * 60); // 5h

        let live_states = LiveWorkerStateRegistry::new();
        register_working_slot(&live_states, 1, &execution_id, &work_item_id);

        let outcome = run_one_pass(&db, &live_states, &EnvelopeThresholds::default(), now());
        assert_eq!(outcome.signaled, 0);
        assert_eq!(outcome.no_envelope_skipped, 1);
    }

    /// `Max` effort is the human unbounded override — never signalled, however
    /// long it runs.
    #[test]
    fn max_effort_has_no_envelope() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "max chore");
        set_effort(&db, &work_item_id, EffortLevel::Max);
        let execution_id = create_execution_started_secs_ago(&db, &work_item_id, 10 * 60 * 60); // 10h

        let live_states = LiveWorkerStateRegistry::new();
        register_working_slot(&live_states, 1, &execution_id, &work_item_id);

        let outcome = run_one_pass(&db, &live_states, &EnvelopeThresholds::default(), now());
        assert_eq!(outcome.signaled, 0);
        assert_eq!(outcome.no_envelope_skipped, 1);
    }

    /// A slot that is not actively `Working` (e.g. still `Spawning`) is not a
    /// candidate — even if its execution is long past any envelope.
    #[test]
    fn non_working_slot_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "spawning chore");
        set_effort(&db, &work_item_id, EffortLevel::Small);
        let execution_id = create_execution_started_secs_ago(&db, &work_item_id, 60 * 60); // 1h

        let live_states = LiveWorkerStateRegistry::new();
        // Register but leave at Spawning (no events applied → not Working).
        live_states.register_spawn(
            1,
            &execution_id,
            "claude-opus-4-8",
            std::process::id() as i32,
            Some(WorkItemBinding {
                work_item_id: work_item_id.clone(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.clone(),
            }),
        );

        let outcome = run_one_pass(&db, &live_states, &EnvelopeThresholds::default(), now());
        assert_eq!(outcome.signaled, 0);
        assert_eq!(outcome.not_working_skipped, 1);
    }

    #[test]
    fn thresholds_from_env_override_defaults_and_disable() {
        let thresholds = EnvelopeThresholds::from_env(|k| match k {
            "BOSS_ENVELOPE_LARGE_SECS" => Some(OsString::from("7200")),
            "BOSS_ENVELOPE_TRIVIAL_SECS" => Some(OsString::from("0")), // disable
            _ => None,
        });
        assert_eq!(thresholds.large, 7200);
        assert_eq!(thresholds.for_effort(EffortLevel::Large), Some(7200));
        assert_eq!(
            thresholds.small, DEFAULT_ENVELOPE_SMALL_SECS,
            "unset var keeps its default"
        );
        assert_eq!(
            thresholds.for_effort(EffortLevel::Trivial),
            None,
            "a <= 0 threshold disables that class",
        );
        assert_eq!(thresholds.for_effort(EffortLevel::Max), None, "max is always unbounded");
    }

    // ─── Timer-wheel subscriber ─────────────────────────────────────────────

    /// The timer-triggered single-slot recheck and the periodic sweep must
    /// share one idempotency guard: whichever path observes the overrun
    /// first files the item, and the other must see `AlreadySignaled`
    /// rather than filing a duplicate — in either order.
    #[test]
    fn timer_and_sweep_paths_share_one_idempotency_guard() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "oversize chore");
        set_effort(&db, &work_item_id, EffortLevel::Small); // 15 min envelope
        let execution_id = create_execution_started_secs_ago(&db, &work_item_id, 20 * 60);

        let live_states = LiveWorkerStateRegistry::new();
        register_working_slot(&live_states, 1, &execution_id, &work_item_id);

        let thresholds = EnvelopeThresholds::default();

        // Timer path fires first...
        handle_timer_deadline(&db, &live_states, &thresholds, &execution_id);
        assert_eq!(
            overrun_items(&db, &execution_id),
            1,
            "timer path files exactly one overrun item"
        );

        // ...then the sweep passes over the same execution: must not duplicate.
        let outcome = run_one_pass(&db, &live_states, &thresholds, now());
        assert_eq!(
            outcome.signaled, 0,
            "sweep must not re-signal what the timer path already filed"
        );
        assert_eq!(outcome.already_signaled, 1);
        assert_eq!(overrun_items(&db, &execution_id), 1, "still exactly one overrun item");

        // Firing the timer path again for the same execution is also a no-op.
        handle_timer_deadline(&db, &live_states, &thresholds, &execution_id);
        assert_eq!(
            overrun_items(&db, &execution_id),
            1,
            "timer path re-fired: still exactly one overrun item"
        );
    }

    /// A `Timer` event for a slot that has since gone terminal (or been
    /// released) is simply a no-op — nothing to look up, nothing filed.
    #[test]
    fn timer_deadline_for_a_no_longer_live_slot_is_a_no_op() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "oversize chore");
        set_effort(&db, &work_item_id, EffortLevel::Small);
        let execution_id = create_execution_started_secs_ago(&db, &work_item_id, 20 * 60);

        // No live slot registered for this execution id at all.
        let live_states = LiveWorkerStateRegistry::new();

        handle_timer_deadline(&db, &live_states, &EnvelopeThresholds::default(), &execution_id);
        assert_eq!(overrun_items(&db, &execution_id), 0);
    }

    /// End-to-end: `spawn_timer_subscriber`'s initial reconcile pass
    /// schedules a timer-wheel deadline for an already-over-envelope live
    /// slot, the wheel fires it almost immediately, and the subscriber
    /// files the overrun item — racing (and not duplicating with) the
    /// periodic sweep, exactly the "bus races the sweep" invariant.
    #[tokio::test]
    async fn timer_subscriber_signals_exactly_once_racing_the_sweep() {
        let (_dir, db) = crate::test_support::open_db_arc();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "oversize chore");
        set_effort(&db, &work_item_id, EffortLevel::Small); // 15 min envelope
        // Started well past the envelope, so the very first scheduled
        // deadline computes to "already due" and fires almost immediately.
        let execution_id = create_execution_started_secs_ago(&db, &work_item_id, 20 * 60);

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_working_slot(&live_states, 1, &execution_id, &work_item_id);

        let event_bus = Arc::new(EventBus::new());
        let timer_wheel = Arc::new(TimerWheel::spawn(event_bus.clone()));

        let _subscriber_handle = spawn_timer_subscriber(
            db.clone(),
            live_states.clone(),
            timer_wheel.clone(),
            event_bus.clone(),
            EnvelopeThresholds::default(),
        );

        tokio::time::timeout(Duration::from_secs(5), async {
            while overrun_items(&db, &execution_id) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timer subscriber must file the overrun item");

        assert_eq!(
            overrun_items(&db, &execution_id),
            1,
            "exactly one overrun item from the timer path"
        );

        // The periodic sweep racing the same execution must not duplicate it.
        let outcome = run_one_pass(&db, &live_states, &EnvelopeThresholds::default(), now());
        assert_eq!(
            outcome.signaled, 0,
            "sweep must not re-signal what the timer path already filed"
        );
        assert_eq!(outcome.already_signaled, 1);
        assert_eq!(overrun_items(&db, &execution_id), 1, "still exactly one overrun item");
    }
}
