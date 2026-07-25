//! Derived per-execution timeline state, and the single definition of
//! "this dispatch stage is stalled".
//!
//! A dispatch timeline can be arbitrarily long, but every stall decision
//! reads exactly three things out of it: the last *real* (non-
//! `stage_stalled`) event, whether that event is terminal, and whether a
//! `stage_stalled` record has already been written for it. [`TimelineState`]
//! is that fixed-size reduction.
//!
//! Both read paths fold into it before deciding:
//!
//! - the file-scan sweeps ([`crate::pending_stalls`],
//!   [`crate::persistently_stalled`]), which walk the per-execution
//!   `dispatch.jsonl` mirrors and work with the engine wedged or absent;
//! - the engine's incremental [`crate::TimelineIndex`], which folds the same
//!   events out of a byte-offset tail of the flat `current.jsonl` stream.
//!
//! Keeping the decision here means those two can never drift: a change to
//! what counts as a stall lands in one place and both paths inherit it.

use std::collections::BTreeMap;
use std::time::Duration;

use boss_dispatch_events::DispatchEvent;

/// Wire stage name of the detector's own output. Folding treats these
/// records as *flags about* a timeline rather than as steps in it — without
/// that exclusion the detector's own writes would look like progress and it
/// would never flag the same wedge twice (and, worse, would reset the
/// elapsed-in-stage clock on every pass).
pub(crate) const STAGE_STALLED: &str = "stage_stalled";

/// Per-stage stalled-detection thresholds. The watchdog used to apply
/// a single global threshold to every stage, but the cube-lease
/// hang incident (`exec_18aec07893bd2e30_29`, 2026-05-12) showed that
/// a 120s default is too coarse for the early dispatch stages — the
/// engine had wedged in `worker_claimed` for 46 seconds without any
/// `stage_stalled` event firing, because the global threshold hadn't
/// elapsed yet. Per-stage overrides let us flag the early handoffs
/// (worker_claimed → cube_repo_ensured → cube_workspace_leased)
/// faster while keeping the longer pane-spawn stages on a generous
/// threshold.
#[derive(Debug, Clone)]
pub struct StageThresholds {
    default_ms: u128,
    overrides: BTreeMap<String, u128>,
}

impl StageThresholds {
    pub fn new(default: Duration) -> Self {
        Self {
            default_ms: default.as_millis(),
            overrides: BTreeMap::new(),
        }
    }

    /// Override the threshold for a specific stage. Pass the wire
    /// stage name (e.g. `"worker_claimed"`) — the watchdog matches
    /// against `DispatchEvent::stage`.
    pub fn with_override(mut self, stage: impl Into<String>, threshold: Duration) -> Self {
        self.overrides.insert(stage.into(), threshold.as_millis());
        self
    }

    pub fn for_stage(&self, stage: &str) -> u128 {
        self.overrides.get(stage).copied().unwrap_or(self.default_ms)
    }

    pub fn default_ms(&self) -> u128 {
        self.default_ms
    }

    /// The largest threshold any stage can carry — the default or the
    /// biggest override, whichever is larger. This is the width of the
    /// decision window: an event younger than this can never be flagged by
    /// any stage's rule, which is what makes an incremental reader's bounded
    /// scan window sound.
    pub fn max_ms(&self) -> u128 {
        self.overrides
            .values()
            .copied()
            .fold(self.default_ms, |acc, ms| acc.max(ms))
    }
}

/// One stage stall the detector wants to surface as a
/// `stage_stalled` event. Carries enough context for the writer to
/// emit a fully-populated `DispatchEvent` without re-reading the
/// timeline.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct StalledStage {
    pub execution_id: String,
    pub work_item_id: Option<String>,
    /// The dispatch stage that hasn't progressed (e.g.
    /// `cube_change_created`). This is the last non-`stage_stalled`
    /// stage in the timeline — a previously-emitted stage_stalled
    /// event doesn't itself count as the stage that's stuck.
    pub stalled_stage: String,
    pub stalled_outcome: String,
    pub last_ts_epoch_ms: u128,
    pub elapsed_in_stage_ms: u128,
}

/// An event is "terminal" — i.e., the dispatch timeline for that
/// execution is officially over — when it is either a successful
/// `pane_spawned` (the slot is up and the worker is now driving),
/// or any explicit error (we won't get a follow-up; the
/// `record_start_failure` / `pane_spawn_failed` paths have run).
pub fn is_terminal_event(event: &DispatchEvent) -> bool {
    if event.outcome == "error" {
        return true;
    }
    if event.stage == "pane_spawned" && event.outcome == "ok" {
        return true;
    }
    false
}

/// The last non-`stage_stalled` event of a timeline, reduced to the fields
/// a stall decision needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LastRealEvent {
    work_item_id: Option<String>,
    stage: String,
    outcome: String,
    ts_epoch_ms: u128,
    terminal: bool,
}

/// Fixed-size reduction of one execution's dispatch timeline.
///
/// Built by folding events in append order via [`TimelineState::apply`].
/// The fold is **idempotent** — re-applying an event already folded in leaves
/// the state unchanged — and its **decisions are suffix-determined**: every
/// answer depends only on the events at or after the last real event.
/// (The state can still carry an older `last_flag`, but such a flag names an
/// earlier event's `(stage, ts)` pair and so matches nothing.) Together those
/// are what let the incremental index re-apply an overlapping byte range, and
/// drop a terminal timeline and rebuild it from a later event, without ever
/// diverging from a full rescan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimelineState {
    last_real: Option<LastRealEvent>,
    /// `(stalled_stage, stalled_at_ts_epoch_ms)` carried by the most recent
    /// `stage_stalled` record. A full rescan asks whether *any* such record
    /// matches the current last real event; keeping only the most recent one
    /// is equivalent, because every `stage_stalled` record written after an
    /// event describes that event — so if any trailing flag matches, the
    /// last one does too.
    last_flag: Option<(String, u128)>,
}

impl TimelineState {
    /// Fold one event into the state. Events are expected in append order;
    /// a real event replaces `last_real` when its timestamp is at least the
    /// incumbent's, which agrees with "last in file order" for the
    /// non-decreasing timestamps the sink writes and stays deterministic if
    /// two concurrent emits ever land out of order.
    pub fn apply(&mut self, event: &DispatchEvent) {
        if event.stage == STAGE_STALLED {
            self.last_flag = Some((flagged_stage(event), flagged_ts(event)));
            return;
        }
        let supersedes = self
            .last_real
            .as_ref()
            .is_none_or(|last| event.ts_epoch_ms >= last.ts_epoch_ms);
        if supersedes {
            self.last_real = Some(LastRealEvent {
                work_item_id: event.work_item_id.clone(),
                stage: event.stage.clone(),
                outcome: event.outcome.clone(),
                ts_epoch_ms: event.ts_epoch_ms,
                terminal: is_terminal_event(event),
            });
        }
    }

    /// Fold a whole timeline at once. Returns `None` for an empty slice so
    /// callers can skip executions with nothing recorded.
    pub fn from_events(events: &[DispatchEvent]) -> Option<Self> {
        if events.is_empty() {
            return None;
        }
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        Some(state)
    }

    /// Whether this timeline can still produce a stall. A timeline whose
    /// last real event is terminal never will, and one with no real event
    /// at all has nothing to attribute a stall to.
    ///
    /// The incremental index drops everything else, which is safe precisely
    /// because the state is suffix-determined: if a dropped execution ever
    /// emits another event, folding that event alone reproduces exactly the
    /// state a full rescan would compute.
    pub fn is_live(&self) -> bool {
        self.last_real.as_ref().is_some_and(|last| !last.terminal)
    }

    /// Milliseconds the current stage has been sitting, relative to
    /// `now_ms`. `None` for a timeline with no real event.
    pub fn elapsed_in_stage_ms(&self, now_ms: u128) -> Option<u128> {
        self.last_real
            .as_ref()
            .map(|last| now_ms.saturating_sub(last.ts_epoch_ms))
    }

    /// The `StalledStage` the `stage_stalled` detector should emit, or
    /// `None` when the timeline is fresh, terminal, or already carries a
    /// `stage_stalled` record for the current stalled stage.
    pub fn stall_to_emit(
        &self,
        execution_id: &str,
        now_ms: u128,
        thresholds: &StageThresholds,
    ) -> Option<StalledStage> {
        let last = self.live_last_real()?;
        let elapsed = now_ms.saturating_sub(last.ts_epoch_ms);
        if elapsed < thresholds.for_stage(&last.stage) {
            return None;
        }
        if self.already_flagged(last) {
            return None;
        }
        Some(self.stalled_stage(execution_id, last, elapsed))
    }

    /// The `StalledStage` the operator-facing escalation sweep should
    /// surface: a flat, much larger threshold and — deliberately — no
    /// dedupe against a prior `stage_stalled` record. See
    /// [`crate::persistently_stalled`] for why re-reporting is the point.
    pub fn persistent_stall(&self, execution_id: &str, now_ms: u128, threshold_ms: u128) -> Option<StalledStage> {
        let last = self.live_last_real()?;
        let elapsed = now_ms.saturating_sub(last.ts_epoch_ms);
        if elapsed < threshold_ms {
            return None;
        }
        Some(self.stalled_stage(execution_id, last, elapsed))
    }

    fn live_last_real(&self) -> Option<&LastRealEvent> {
        let last = self.last_real.as_ref()?;
        if last.terminal { None } else { Some(last) }
    }

    fn already_flagged(&self, last: &LastRealEvent) -> bool {
        self.last_flag
            .as_ref()
            .is_some_and(|(stage, ts)| stage == &last.stage && *ts == last.ts_epoch_ms)
    }

    fn stalled_stage(&self, execution_id: &str, last: &LastRealEvent, elapsed: u128) -> StalledStage {
        StalledStage {
            execution_id: execution_id.to_owned(),
            work_item_id: last.work_item_id.clone(),
            stalled_stage: last.stage.clone(),
            stalled_outcome: last.outcome.clone(),
            last_ts_epoch_ms: last.ts_epoch_ms,
            elapsed_in_stage_ms: elapsed,
        }
    }
}

/// `details.stalled_stage` of a `stage_stalled` record. A record missing the
/// key yields `""`, which matches no real stage name, so a malformed flag
/// never suppresses a genuine stall.
fn flagged_stage(event: &DispatchEvent) -> String {
    event
        .details
        .get("stalled_stage")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}

/// `details.stalled_at_ts_epoch_ms` of a `stage_stalled` record. A record
/// missing the key yields `u128::MAX`, a timestamp no real event can carry,
/// so — as with [`flagged_stage`] — a malformed flag fails open.
fn flagged_ts(event: &DispatchEvent) -> u128 {
    event
        .details
        .get("stalled_at_ts_epoch_ms")
        .and_then(|v| v.as_u64())
        .map_or(u128::MAX, |ts| ts as u128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_dispatch_events::{Outcome, Stage};

    fn ev(stage: Stage, outcome: Outcome, ts: u128) -> DispatchEvent {
        let mut event = DispatchEvent::new(stage, outcome, "exec-1");
        event.ts_epoch_ms = ts;
        event
    }

    fn flag(stalled_stage: &str, stalled_at: u128, ts: u128) -> DispatchEvent {
        let mut event = ev(Stage::StageStalled, Outcome::Ok, ts);
        event.details = serde_json::json!({
            "stalled_stage": stalled_stage,
            "stalled_outcome": "ok",
            "stalled_at_ts_epoch_ms": stalled_at as u64,
        });
        event
    }

    fn flat(ms: u64) -> StageThresholds {
        StageThresholds::new(Duration::from_millis(ms))
    }

    #[test]
    fn stage_stalled_records_never_count_as_progress() {
        let mut state = TimelineState::default();
        state.apply(&ev(Stage::CubeChangeCreated, Outcome::Ok, 1_000));
        state.apply(&flag("cube_change_created", 1_000, 6_500));

        // The stalled clock still runs from the real event at t=1000, not
        // from the flag at t=6500.
        assert_eq!(state.elapsed_in_stage_ms(10_000), Some(9_000));
    }

    #[test]
    fn stall_to_emit_fires_once_threshold_passes() {
        let mut state = TimelineState::default();
        state.apply(&ev(Stage::CubeChangeCreated, Outcome::Ok, 1_000));

        assert!(state.stall_to_emit("exec-1", 5_000, &flat(5_000)).is_none());
        let stall = state.stall_to_emit("exec-1", 10_000, &flat(5_000)).unwrap();
        assert_eq!(stall.stalled_stage, "cube_change_created");
        assert_eq!(stall.elapsed_in_stage_ms, 9_000);
    }

    #[test]
    fn stall_to_emit_dedupes_against_matching_flag() {
        let mut state = TimelineState::default();
        state.apply(&ev(Stage::CubeChangeCreated, Outcome::Ok, 1_000));
        state.apply(&flag("cube_change_created", 1_000, 6_500));

        assert!(state.stall_to_emit("exec-1", 10_000, &flat(5_000)).is_none());
    }

    #[test]
    fn stall_to_emit_re_fires_once_the_stage_advances_past_a_flag() {
        let mut state = TimelineState::default();
        state.apply(&ev(Stage::CubeWorkspaceLeased, Outcome::Ok, 1_000));
        state.apply(&flag("cube_workspace_leased", 1_000, 6_500));
        state.apply(&ev(Stage::CubeChangeCreated, Outcome::Ok, 7_000));

        let stall = state.stall_to_emit("exec-1", 15_000, &flat(5_000)).unwrap();
        assert_eq!(stall.stalled_stage, "cube_change_created");
    }

    /// A flag naming the right stage but the wrong timestamp describes an
    /// *earlier* visit to that stage and must not suppress the current one.
    #[test]
    fn stall_to_emit_ignores_a_flag_for_the_same_stage_at_a_different_timestamp() {
        let mut state = TimelineState::default();
        state.apply(&ev(Stage::CubeChangeCreated, Outcome::Ok, 1_000));
        state.apply(&flag("cube_change_created", 1_000, 6_500));
        state.apply(&ev(Stage::CubeChangeCreated, Outcome::Ok, 20_000));

        let stall = state.stall_to_emit("exec-1", 30_000, &flat(5_000)).unwrap();
        assert_eq!(stall.last_ts_epoch_ms, 20_000);
    }

    /// A `stage_stalled` record whose `details` lost its keys must fail
    /// open — suppressing a real stall on a malformed flag would silently
    /// blind the detector.
    #[test]
    fn stall_to_emit_fails_open_on_a_flag_with_missing_details() {
        let mut state = TimelineState::default();
        state.apply(&ev(Stage::CubeChangeCreated, Outcome::Ok, 1_000));
        let mut bare = ev(Stage::StageStalled, Outcome::Ok, 6_500);
        bare.details = serde_json::Value::Null;
        state.apply(&bare);

        assert!(state.stall_to_emit("exec-1", 10_000, &flat(5_000)).is_some());
    }

    #[test]
    fn terminal_timelines_are_neither_live_nor_stalled() {
        let mut state = TimelineState::default();
        state.apply(&ev(Stage::PaneSpawned, Outcome::Ok, 1_000));
        assert!(!state.is_live());
        assert!(state.stall_to_emit("exec-1", 9_999_999, &flat(5_000)).is_none());
        assert!(state.persistent_stall("exec-1", 9_999_999, 5_000).is_none());

        let mut errored = TimelineState::default();
        errored.apply(&ev(Stage::RunStarted, Outcome::Error, 1_000));
        assert!(!errored.is_live());
    }

    #[test]
    fn persistent_stall_ignores_prior_flags() {
        let mut state = TimelineState::default();
        state.apply(&ev(Stage::CubeChangeCreated, Outcome::Ok, 0));
        state.apply(&flag("cube_change_created", 0, 6_500));

        // `stall_to_emit` dedupes on the flag; `persistent_stall` must not.
        assert!(state.stall_to_emit("exec-1", 400_000, &flat(300_000)).is_none());
        let stall = state.persistent_stall("exec-1", 400_000, 300_000).unwrap();
        assert_eq!(stall.elapsed_in_stage_ms, 400_000);
    }

    /// The fold must be idempotent: the incremental index re-applies an
    /// overlapping byte range whenever it re-anchors its cursor, so folding
    /// the same event twice has to be indistinguishable from folding it once.
    #[test]
    fn fold_is_idempotent_under_replay() {
        let events = vec![
            ev(Stage::RequestRecorded, Outcome::Ok, 0),
            ev(Stage::WorkerClaimed, Outcome::Ok, 1_000),
            flag("worker_claimed", 1_000, 6_000),
        ];
        let once = TimelineState::from_events(&events).unwrap();

        let mut twice = TimelineState::default();
        for event in events.iter().chain(events.iter()) {
            twice.apply(event);
        }
        assert_eq!(once, twice);
    }

    /// Every decision must be determined by the suffix at or after the last
    /// real event: nothing before it can change an answer. This is what makes
    /// it safe for the index to drop a terminal timeline and rebuild it from
    /// a single later event.
    ///
    /// Note the state itself is *not* byte-identical — the full fold still
    /// carries the older `last_flag`. That flag is inert: it names an earlier
    /// event's `(stage, ts)` pair, so it can never match the newer last real
    /// event, which is exactly why it changes no decision.
    #[test]
    fn decisions_are_determined_by_the_suffix_after_the_last_real_event() {
        let full = vec![
            ev(Stage::RequestRecorded, Outcome::Ok, 0),
            flag("request_recorded", 0, 500),
            ev(Stage::PaneSpawned, Outcome::Error, 1_000),
            ev(Stage::RequestRecorded, Outcome::Ok, 2_000),
        ];
        let suffix = vec![ev(Stage::RequestRecorded, Outcome::Ok, 2_000)];

        let full = TimelineState::from_events(&full).unwrap();
        let suffix = TimelineState::from_events(&suffix).unwrap();

        assert_eq!(full.is_live(), suffix.is_live());
        assert_eq!(full.elapsed_in_stage_ms(10_000), suffix.elapsed_in_stage_ms(10_000));
        assert_eq!(
            full.stall_to_emit("exec-1", 10_000, &flat(5_000)),
            suffix.stall_to_emit("exec-1", 10_000, &flat(5_000))
        );
        assert!(full.stall_to_emit("exec-1", 10_000, &flat(5_000)).is_some());
        assert_eq!(
            full.persistent_stall("exec-1", 10_000, 5_000),
            suffix.persistent_stall("exec-1", 10_000, 5_000)
        );
    }

    #[test]
    fn from_events_returns_none_for_an_empty_timeline() {
        assert!(TimelineState::from_events(&[]).is_none());
    }

    /// A timeline containing only detector output has no stage to blame and
    /// must not produce a stall.
    #[test]
    fn flag_only_timeline_has_no_stall() {
        let state = TimelineState::from_events(&[flag("cube_change_created", 1_000, 6_500)]).unwrap();
        assert!(!state.is_live());
        assert!(state.stall_to_emit("exec-1", 9_999_999, &flat(5_000)).is_none());
        assert!(state.persistent_stall("exec-1", 9_999_999, 5_000).is_none());
    }

    #[test]
    fn stage_thresholds_falls_back_to_default_for_unknown_stages() {
        let t = StageThresholds::new(Duration::from_secs(120)).with_override("worker_claimed", Duration::from_secs(30));
        assert_eq!(t.for_stage("worker_claimed"), 30_000);
        assert_eq!(t.for_stage("cube_repo_ensured"), 120_000);
        assert_eq!(t.for_stage("anything_else"), 120_000);
    }

    #[test]
    fn stage_thresholds_max_ms_spans_default_and_overrides() {
        let default_wins =
            StageThresholds::new(Duration::from_secs(120)).with_override("worker_claimed", Duration::from_secs(30));
        assert_eq!(default_wins.max_ms(), 120_000);

        let override_wins =
            StageThresholds::new(Duration::from_secs(30)).with_override("pane_spawned", Duration::from_secs(600));
        assert_eq!(override_wins.max_ms(), 600_000);
    }

    #[test]
    fn is_terminal_event_recognises_terminal_shapes() {
        assert!(!is_terminal_event(&ev(Stage::RequestRecorded, Outcome::Ok, 0)));
        assert!(is_terminal_event(&ev(Stage::PaneSpawned, Outcome::Ok, 0)));
        assert!(is_terminal_event(&ev(Stage::RunStarted, Outcome::Error, 0)));
        assert!(is_terminal_event(&ev(Stage::PaneSpawned, Outcome::Error, 0)));
    }
}
