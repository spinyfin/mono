//! The `boothby_scheduler` loop — Boothby's timer + event activation and
//! `boothby_passes` lifecycle (`tools/boss/docs/designs/boothby.md`
//! §"Activation model" / §"Pass lifecycle").
//!
//! Modeled on [`crate::automation_scheduler`]'s adaptive-sleep pattern, but
//! simpler in one respect: Boothby is a singleton (no per-row schedule to
//! iterate), so there is no persisted `next_due_at` column to advance.
//! Instead the cron anchor is derived purely from pass history —
//! [`crate::work::WorkDb::last_boothby_schedule_pass_started_at`] — which
//! keeps passes stateless-by-design (design §"Idempotence & convergence"):
//! there is no separate mutable schedule row that could drift out of sync
//! with reality.
//!
//! ## What this module does NOT do yet
//!
//! The brief composer (design task 4) and session spawn (task 5) are not
//! wired up. [`NothingToDoPassRunner`] is the placeholder seam — precisely
//! [`crate::automation_scheduler::LoggingTriageDispatcher`]'s role for
//! automations — so every pass concludes `nothing_to_do` without spawning a
//! session, which is exactly the design's pre-spawn short-circuit degenerate
//! case (no candidates yet exist because nothing computes them yet). Once
//! task 4/5 land, a real [`BoothbyPassRunner`] replaces this placeholder and
//! the scheduler itself needs no changes.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use boss_protocol::{
    BOOTHBY_OUTCOME_COMPLETED, BOOTHBY_OUTCOME_FAILED, BOOTHBY_OUTCOME_NOTHING_TO_DO, BOOTHBY_OUTCOME_TIMED_OUT,
    BOOTHBY_TRIGGER_SCHEDULE, BoothbyPass,
};

use crate::automation_schedule::{next_occurrence_after, parse_cron, parse_timezone};
use crate::boothby_events::BoothbyEventQueue;
use crate::settings::SettingsStore;
use crate::work::WorkDb;

/// Maximum time the scheduler sleeps between ticks. Caps the sleep so the
/// loop wakes at least hourly as a safety net even with `boothby.mode = off`
/// or a schedule far in the future.
pub const BOOTHBY_SCHEDULER_MAX_SLEEP_SECS: u64 = 3600;

pub const SETTING_MODE: &str = "boothby.mode";
pub const SETTING_SCHEDULE: &str = "boothby.schedule";
pub const SETTING_EVENT_DELAY_SECS: &str = "boothby.event_delay_secs";
pub const SETTING_MIN_PASS_GAP_SECS: &str = "boothby.min_pass_gap_secs";
pub const SETTING_PASS_TIMEOUT_SECS: &str = "boothby.pass_timeout_secs";

pub const BOOTHBY_MODE_OFF: &str = "off";
pub const BOOTHBY_MODE_PROPOSE: &str = "propose";
pub const BOOTHBY_MODE_AUTO: &str = "auto";

/// `true` for the three modes the design's `boothby.mode` setting allows.
/// Used by `SetBoothbyMode` to reject a typo before it gets persisted.
pub fn is_valid_boothby_mode(mode: &str) -> bool {
    matches!(mode, BOOTHBY_MODE_OFF | BOOTHBY_MODE_PROPOSE | BOOTHBY_MODE_AUTO)
}

fn current_mode(settings: &SettingsStore) -> String {
    settings
        .get_text(SETTING_MODE)
        .unwrap_or_else(|| BOOTHBY_MODE_PROPOSE.to_owned())
}

/// What a [`BoothbyPassRunner`] reports back to the scheduler once a pass
/// has run. Maps onto `boothby_passes.outcome`, minus `timed_out` (the
/// scheduler itself detects that by racing the runner against
/// `boothby.pass_timeout_secs`, so a runner never needs to self-report it)
/// and `capped` (the executor's job, once task 2 lands).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoothbyRunOutcome {
    /// The pre-spawn short-circuit found no candidates; no session was
    /// spawned.
    NothingToDo,
    /// The pass ran (a session was spawned and reached a decision).
    Completed { summary: Option<String> },
    /// The pass could not complete for a reason worth recording.
    Failed { detail: String },
}

/// The pass-execution seam. The scheduler calls this once it has decided a
/// pass should run (schedule due, event due, or a manual fire) and a
/// `boothby_passes` row is already open. Implemented for real once the brief
/// composer (task 4) and session spawn (task 5) land; [`NothingToDoPassRunner`]
/// is the task-3 placeholder.
#[async_trait]
pub trait BoothbyPassRunner: Send + Sync {
    async fn run_pass(&self, pass: &BoothbyPass) -> BoothbyRunOutcome;
}

/// Task-3 placeholder runner: the brief composer and session spawn are
/// tasks 4/5. Until they land, every pass reports `nothing_to_do` — which is
/// simply true today (no candidates are computed yet), not a faked result.
/// Mirrors [`crate::automation_scheduler::LoggingTriageDispatcher`]'s role
/// for automations.
#[derive(Debug, Default)]
pub struct NothingToDoPassRunner;

#[async_trait]
impl BoothbyPassRunner for NothingToDoPassRunner {
    async fn run_pass(&self, pass: &BoothbyPass) -> BoothbyRunOutcome {
        tracing::debug!(
            pass_id = %pass.id,
            trigger = %pass.trigger,
            "boothby: pass brief composer / session spawn not yet implemented \
             (design tasks 4/5); reporting nothing_to_do",
        );
        BoothbyRunOutcome::NothingToDo
    }
}

/// Notified whenever a `boothby_passes` row opens or closes, so the engine
/// can push it on the `boothby.activity` topic. Decoupled from `ServerState`
/// (which lives in `app.rs`) the same way [`crate::automation_scheduler`]'s
/// `TriageDispatcher` is decoupled from it — this module only knows about
/// `WorkDb` and trait objects.
#[async_trait]
pub trait BoothbyActivitySink: Send + Sync {
    async fn pass_changed(&self, pass: &BoothbyPass);
}

/// Default sink for tests and any caller that doesn't want topic pushes.
#[derive(Debug, Default, Clone)]
pub struct NoopBoothbyActivitySink;

#[async_trait]
impl BoothbyActivitySink for NoopBoothbyActivitySink {
    async fn pass_changed(&self, _pass: &BoothbyPass) {}
}

/// Result of one scheduler tick, for logging and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoothbyTickOutcome {
    /// `boothby.mode = off`; the tick did nothing.
    ModeOff,
    /// A pass was already open and not yet past `boothby.pass_timeout_secs`
    /// (crash-recovery case only — today's synchronous runner never leaves
    /// one open across a tick boundary).
    AlreadyOpen,
    /// A trigger is due, but `boothby.min_pass_gap_secs` hasn't elapsed
    /// since the last pass of any trigger.
    GapBlocked,
    /// Neither the schedule nor the event queue has anything due.
    NotDue,
    /// A pass ran to a terminal outcome.
    Fired { pass_id: String, outcome: String },
    /// A trigger was due and the gap had elapsed, but opening or finishing
    /// the pass failed at the DB layer (logged; surfaced here for tests).
    FireFailed,
}

/// The cron occurrence Boothby is next due to fire on a schedule trigger, or
/// `None` if `boothby.schedule` doesn't parse. Anchored on the most recent
/// `trigger = 'schedule'` pass — see the module doc for why there is no
/// persisted `next_due_at`.
///
/// Falls back to the fixed epoch `0` (not `now`) when Boothby has never
/// fired a scheduled pass. `now` would be wrong here: `next_occurrence_after`
/// returns the occurrence *strictly after* its anchor, so anchoring on the
/// same instant this result is compared against would make `due` chase `now`
/// forever without ever becoming `<= now` — a fresh install would never
/// auto-fire. Anchoring on a fixed point in the past instead makes the first
/// occurrence due immediately, the same "catch up promptly" behavior
/// `automation_scheduler` gets from initializing `next_due_at` once at
/// creation time and then only ever advancing it forward from a real fire.
fn scheduled_next_due(work_db: &WorkDb, settings: &SettingsStore, _now: i64) -> Option<i64> {
    let cron_expr = settings.get_text(SETTING_SCHEDULE)?;
    let schedule = parse_cron(&cron_expr).ok()?;
    let tz = parse_timezone("UTC").ok()?;
    let anchor = work_db
        .last_boothby_schedule_pass_started_at()
        .ok()
        .flatten()
        .unwrap_or(0);
    next_occurrence_after(&schedule, tz, anchor)
}

/// Run the placed pass to completion (racing it against
/// `boothby.pass_timeout_secs`) and finish the row. Shared by the scheduler
/// tick and the manual `RunBoothbyPass` RPC handler, so a manual fire
/// records exactly the same lifecycle a scheduled one does.
///
/// Returns `None` (rather than erroring) when a pass is already open — an
/// ordinary race with another caller, not a failure — or when the DB layer
/// itself errors (logged).
pub async fn run_and_finish_pass(
    work_db: &WorkDb,
    runner: &dyn BoothbyPassRunner,
    activity: &dyn BoothbyActivitySink,
    trigger: &str,
    now_epoch: i64,
    pass_timeout_secs: i64,
) -> Option<BoothbyPass> {
    let opened = match work_db.open_boothby_pass(trigger, &now_epoch.to_string()) {
        Ok(Some(pass)) => pass,
        Ok(None) => return None,
        Err(err) => {
            tracing::warn!(?err, trigger, "boothby: failed to open pass; skipping");
            return None;
        }
    };
    activity.pass_changed(&opened).await;

    let run_result = tokio::time::timeout(
        Duration::from_secs(pass_timeout_secs.max(1) as u64),
        runner.run_pass(&opened),
    )
    .await;

    let (outcome_str, summary): (&str, Option<String>) = match run_result {
        Ok(BoothbyRunOutcome::NothingToDo) => (BOOTHBY_OUTCOME_NOTHING_TO_DO, None),
        Ok(BoothbyRunOutcome::Completed { summary }) => (BOOTHBY_OUTCOME_COMPLETED, summary),
        Ok(BoothbyRunOutcome::Failed { detail }) => (BOOTHBY_OUTCOME_FAILED, Some(detail)),
        Err(_elapsed) => {
            tracing::warn!(pass_id = %opened.id, trigger, pass_timeout_secs, "boothby: pass timed out");
            (BOOTHBY_OUTCOME_TIMED_OUT, None)
        }
    };

    let finish_now = boss_engine_utils::epoch_time::now_epoch_secs().to_string();
    match work_db.finish_boothby_pass(&opened.id, &finish_now, outcome_str, summary.as_deref(), None, None) {
        Ok(finished) => {
            activity.pass_changed(&finished).await;
            Some(finished)
        }
        Err(err) => {
            tracing::warn!(?err, pass_id = %opened.id, "boothby: failed to finish pass");
            None
        }
    }
}

/// One scheduler tick, pure of wall-clock reads so it's deterministically
/// testable. Decides whether a pass should fire this tick and, if so, runs
/// it through [`run_and_finish_pass`].
pub async fn run_one_tick(
    work_db: &WorkDb,
    settings: &SettingsStore,
    events: &BoothbyEventQueue,
    runner: &dyn BoothbyPassRunner,
    activity: &dyn BoothbyActivitySink,
    now: i64,
) -> BoothbyTickOutcome {
    if current_mode(settings) == BOOTHBY_MODE_OFF {
        return BoothbyTickOutcome::ModeOff;
    }

    // Crash recovery: a pass left open past its timeout (engine restarted
    // mid-pass) is force-closed before we consider firing a new one. With
    // today's synchronous runner this branch is unreachable outside a
    // crash — the tick that opens a pass also finishes it — but the check
    // is cheap and becomes load-bearing once task 5 spawns a real session
    // that can outlive an engine restart.
    if let Ok(Some(open)) = work_db.get_open_boothby_pass() {
        let started: i64 = open.started_at.parse().unwrap_or(now);
        let timeout = settings.get_text_i64(SETTING_PASS_TIMEOUT_SECS).unwrap_or(900);
        if now - started <= timeout {
            return BoothbyTickOutcome::AlreadyOpen;
        }
        if let Ok(finished) =
            work_db.finish_boothby_pass(&open.id, &now.to_string(), BOOTHBY_OUTCOME_TIMED_OUT, None, None, None)
        {
            activity.pass_changed(&finished).await;
        }
    }

    let min_gap = settings.get_text_i64(SETTING_MIN_PASS_GAP_SECS).unwrap_or(900);
    let gap_remaining = work_db
        .last_boothby_pass_started_at()
        .ok()
        .flatten()
        .map(|last| (last + min_gap - now).max(0))
        .unwrap_or(0);
    if gap_remaining > 0 {
        return BoothbyTickOutcome::GapBlocked;
    }

    let event_delay = settings.get_text_i64(SETTING_EVENT_DELAY_SECS).unwrap_or(300);
    let trigger = if let Some(name) = events.take_due(now, event_delay) {
        name
    } else if scheduled_next_due(work_db, settings, now).is_some_and(|due| now >= due) {
        BOOTHBY_TRIGGER_SCHEDULE.to_owned()
    } else {
        return BoothbyTickOutcome::NotDue;
    };

    let pass_timeout = settings.get_text_i64(SETTING_PASS_TIMEOUT_SECS).unwrap_or(900);
    match run_and_finish_pass(work_db, runner, activity, &trigger, now, pass_timeout).await {
        Some(pass) => BoothbyTickOutcome::Fired {
            pass_id: pass.id,
            outcome: pass.outcome.unwrap_or_default(),
        },
        None => BoothbyTickOutcome::FireFailed,
    }
}

/// How long the scheduler should sleep before its next tick: the earliest of
/// the event queue's debounce, the next cron occurrence, and
/// `boothby.min_pass_gap_secs` since the last pass — clamped to
/// `[1, BOOTHBY_SCHEDULER_MAX_SLEEP_SECS]`. `boothby.mode = off` always
/// sleeps the maximum (an immediate wake comes via `kick` when
/// `SetBoothbyMode` flips it back on).
pub fn next_sleep_secs(work_db: &WorkDb, settings: &SettingsStore, events: &BoothbyEventQueue, now: i64) -> u64 {
    if current_mode(settings) == BOOTHBY_MODE_OFF {
        return BOOTHBY_SCHEDULER_MAX_SLEEP_SECS;
    }

    let event_delay = settings.get_text_i64(SETTING_EVENT_DELAY_SECS).unwrap_or(300);
    let event_wait = events.seconds_until_due(now, event_delay);

    let schedule_wait = scheduled_next_due(work_db, settings, now).map(|due| (due - now).max(0));

    let earliest_trigger = [event_wait, schedule_wait].into_iter().flatten().min();

    let min_gap = settings.get_text_i64(SETTING_MIN_PASS_GAP_SECS).unwrap_or(900);
    let gap_wait = work_db
        .last_boothby_pass_started_at()
        .ok()
        .flatten()
        .map(|last| (last + min_gap - now).max(0))
        .unwrap_or(0);

    let wait = earliest_trigger
        .unwrap_or(BOOTHBY_SCHEDULER_MAX_SLEEP_SECS as i64)
        .max(gap_wait);
    wait.clamp(1, BOOTHBY_SCHEDULER_MAX_SLEEP_SECS as i64) as u64
}

/// Spawn the scheduler loop. Each tick calls [`run_one_tick`] then sleeps
/// for [`next_sleep_secs`], woken early by `kick` — armed by
/// [`BoothbyEventQueue::arm`] (via `events.kick_handle()`, shared with
/// `kick`) and by the manual-run / mode-change RPC handlers.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    settings: Arc<SettingsStore>,
    events: Arc<BoothbyEventQueue>,
    kick: Arc<Notify>,
    runner: Arc<dyn BoothbyPassRunner>,
    activity: Arc<dyn BoothbyActivitySink>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            let outcome = run_one_tick(
                work_db.as_ref(),
                settings.as_ref(),
                events.as_ref(),
                runner.as_ref(),
                activity.as_ref(),
                now,
            )
            .await;
            if !matches!(outcome, BoothbyTickOutcome::ModeOff | BoothbyTickOutcome::NotDue) {
                tracing::info!(?outcome, "boothby scheduler: tick complete");
            }
            let sleep_secs = next_sleep_secs(work_db.as_ref(), settings.as_ref(), events.as_ref(), now);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {}
                _ = kick.notified() => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_db;
    use tempfile::TempDir;

    fn settings_store(tmp: &TempDir) -> SettingsStore {
        let store = SettingsStore::new(tmp.path().join("settings.toml"));
        store.load().unwrap();
        store
    }

    fn queue() -> Arc<BoothbyEventQueue> {
        BoothbyEventQueue::new(Arc::new(Notify::new()))
    }

    #[tokio::test]
    async fn mode_off_short_circuits_the_tick() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        settings.set_text("boothby.mode", "off".to_owned()).unwrap();
        let events = queue();

        let outcome = run_one_tick(
            &db,
            &settings,
            &events,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            1_000,
        )
        .await;
        assert_eq!(outcome, BoothbyTickOutcome::ModeOff);
        assert!(db.list_boothby_passes(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn nothing_due_yet_does_not_fire() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        // Cron far in the future relative to `now` below: use a schedule
        // anchored at `now` itself, so the "next" occurrence is strictly
        // after it and not yet due.
        settings.set_text("boothby.schedule", "0 0 1 1 *".to_owned()).unwrap(); // once a year
        let events = queue();

        let outcome = run_one_tick(
            &db,
            &settings,
            &events,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            1_000,
        )
        .await;
        assert_eq!(outcome, BoothbyTickOutcome::NotDue);
    }

    #[tokio::test]
    async fn schedule_due_fires_a_pass_and_records_nothing_to_do() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        // Every minute — guaranteed due immediately relative to epoch 0 anchor.
        settings.set_text("boothby.schedule", "* * * * *".to_owned()).unwrap();
        let events = queue();

        let outcome = run_one_tick(
            &db,
            &settings,
            &events,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            120,
        )
        .await;
        match outcome {
            BoothbyTickOutcome::Fired { outcome, .. } => assert_eq!(outcome, BOOTHBY_OUTCOME_NOTHING_TO_DO),
            other => panic!("expected Fired, got {other:?}"),
        }
        let passes = db.list_boothby_passes(10).unwrap();
        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].trigger, BOOTHBY_TRIGGER_SCHEDULE);
        assert_eq!(passes[0].outcome.as_deref(), Some(BOOTHBY_OUTCOME_NOTHING_TO_DO));
    }

    #[tokio::test]
    async fn min_pass_gap_blocks_a_second_fire_too_soon() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        settings.set_text("boothby.schedule", "* * * * *".to_owned()).unwrap();
        settings
            .set_text("boothby.min_pass_gap_secs", "900".to_owned())
            .unwrap();
        let events = queue();

        let first = run_one_tick(
            &db,
            &settings,
            &events,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            120,
        )
        .await;
        assert!(matches!(first, BoothbyTickOutcome::Fired { .. }));

        // 60s later — well inside the 900s gap.
        let second = run_one_tick(
            &db,
            &settings,
            &events,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            180,
        )
        .await;
        assert_eq!(second, BoothbyTickOutcome::GapBlocked);
        assert_eq!(
            db.list_boothby_passes(10).unwrap().len(),
            1,
            "no second pass must be recorded"
        );
    }

    #[tokio::test]
    async fn event_trigger_fires_a_pass_named_after_the_event() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        // Push the schedule far out so only the event can fire.
        settings.set_text("boothby.schedule", "0 0 1 1 *".to_owned()).unwrap();
        settings.set_text("boothby.event_delay_secs", "300".to_owned()).unwrap();
        let events = queue();
        events.arm(700, "event:dead_pid_reconcile");

        // Before the debounce elapses: not due.
        let too_soon = run_one_tick(
            &db,
            &settings,
            &events,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            800,
        )
        .await;
        assert_eq!(too_soon, BoothbyTickOutcome::NotDue);

        // After the debounce: fires, named after the event.
        let fired = run_one_tick(
            &db,
            &settings,
            &events,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            1_010,
        )
        .await;
        assert!(matches!(fired, BoothbyTickOutcome::Fired { .. }));
        let passes = db.list_boothby_passes(10).unwrap();
        assert_eq!(passes[0].trigger, "event:dead_pid_reconcile");
    }

    #[tokio::test]
    async fn an_already_open_pass_within_timeout_blocks_the_tick() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();
        let events = queue();

        let outcome = run_one_tick(
            &db,
            &settings,
            &events,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            1_100, // well within the 900s default timeout
        )
        .await;
        assert_eq!(outcome, BoothbyTickOutcome::AlreadyOpen);
    }

    #[tokio::test]
    async fn a_stale_open_pass_past_timeout_is_force_closed_then_a_new_one_can_fire() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        settings.set_text("boothby.pass_timeout_secs", "60".to_owned()).unwrap();
        settings.set_text("boothby.min_pass_gap_secs", "0".to_owned()).unwrap();
        settings.set_text("boothby.schedule", "* * * * *".to_owned()).unwrap();
        let stale = db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();
        let events = queue();

        let outcome = run_one_tick(
            &db,
            &settings,
            &events,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            1_200, // 200s later, past the 60s timeout
        )
        .await;
        assert!(matches!(outcome, BoothbyTickOutcome::Fired { .. }), "{outcome:?}");

        let reloaded_stale = db.get_boothby_pass(&stale.id).unwrap().unwrap();
        assert_eq!(reloaded_stale.outcome.as_deref(), Some(BOOTHBY_OUTCOME_TIMED_OUT));
        assert_eq!(db.list_boothby_passes(10).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn manual_trigger_via_run_and_finish_pass_bypasses_schedule_and_gap() {
        let (_d, db) = open_db();
        // No settings involved at all: manual fire is a direct call, same
        // path the RunBoothbyPass RPC handler uses.
        let outcome = run_and_finish_pass(
            &db,
            &NothingToDoPassRunner,
            &NoopBoothbyActivitySink,
            boss_protocol::BOOTHBY_TRIGGER_MANUAL,
            1_000,
            900,
        )
        .await;
        let pass = outcome.expect("manual fire should open and finish a pass");
        assert_eq!(pass.trigger, boss_protocol::BOOTHBY_TRIGGER_MANUAL);
        assert_eq!(pass.outcome.as_deref(), Some(BOOTHBY_OUTCOME_NOTHING_TO_DO));
    }

    #[test]
    fn next_sleep_secs_is_max_when_mode_is_off() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        settings.set_text("boothby.mode", "off".to_owned()).unwrap();
        let events = queue();
        assert_eq!(
            next_sleep_secs(&db, &settings, &events, 1_000),
            BOOTHBY_SCHEDULER_MAX_SLEEP_SECS
        );
    }

    #[test]
    fn next_sleep_secs_targets_the_armed_event_debounce() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        settings.set_text("boothby.schedule", "0 0 1 1 *".to_owned()).unwrap();
        settings.set_text("boothby.event_delay_secs", "300".to_owned()).unwrap();
        let events = queue();
        events.arm(1_000, "event:dead_pid_reconcile");

        assert_eq!(next_sleep_secs(&db, &settings, &events, 1_100), 200);
    }

    #[test]
    fn next_sleep_secs_respects_min_pass_gap_over_an_already_due_trigger() {
        let (_d, db) = open_db();
        let tmp = TempDir::new().unwrap();
        let settings = settings_store(&tmp);
        settings.set_text("boothby.schedule", "* * * * *".to_owned()).unwrap();
        settings
            .set_text("boothby.min_pass_gap_secs", "900".to_owned())
            .unwrap();
        let pass = db.open_boothby_pass(BOOTHBY_TRIGGER_SCHEDULE, "1000").unwrap().unwrap();
        db.finish_boothby_pass(&pass.id, "1000", BOOTHBY_OUTCOME_NOTHING_TO_DO, None, None, None)
            .unwrap();
        let events = queue();

        // The cron schedule is due every minute (already satisfied), but the
        // gap since the pass at 1000 must still dominate.
        let sleep = next_sleep_secs(&db, &settings, &events, 1_100);
        assert_eq!(sleep, 1000 + 900 - 1_100);
    }
}
