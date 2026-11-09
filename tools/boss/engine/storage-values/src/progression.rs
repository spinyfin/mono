//! Progression: the administrative agents that diagnose and unstick a
//! non-parked work item with no live work. Shared vocabulary and the limits
//! that bound them. See
//! `tools/boss/docs/designs/task-persistence-parking-and-autonomous-progression.md`
//! (sections 2.1, 2.5, 2.6).
//!
//! ## Where the limits come from
//!
//! Every value in [`ProgressionSettings`] defaults to the **placeholder** from
//! the design's section 2.6 table. The placeholders exist so the feature can be
//! exercised in a test instance; they are not yet tuned from measured traffic.
//! None of them may be read as a measured number.
//!
//! The values the candidate view itself needs ([`ProgressionSettings::stuck_after_secs`],
//! [`ProgressionSettings::cooldown_secs`], and the create-budget pair) are
//! published by the engine into the `metadata` table, so every reader of the
//! view — the sweep, the stuck report, `bossctl dispatch diagnose` (which reads
//! `state.db` directly, with the engine down) — applies the same numbers.

/// Feature flag gating the whole feature (default off). With it off nothing is
/// minted and no progression pool slot is ever used; the parking tasks and the
/// stuck report work without it.
pub const PROGRESSION_AGENTS_FLAG: &str = "progression_agents";

/// `work_executions.created_via` stamped on an execution the `progression_sweep`
/// minted.
pub const CREATED_VIA_PROGRESSION_SWEEP: &str = "progression_sweep";

/// `metadata` key holding the published `progression.stuck_after` grace.
pub const METADATA_KEY_STUCK_AFTER_SECS: &str = "progression.stuck_after_secs";

/// `metadata` key holding the published `progression.cooldown`.
pub const METADATA_KEY_COOLDOWN_SECS: &str = "progression.cooldown_secs";

/// `metadata` key holding the published `progression.max_creates_per_item_window`.
pub const METADATA_KEY_MAX_CREATES_PER_ITEM_WINDOW: &str = "progression.max_creates_per_item_window";

/// `metadata` key holding the published rolling window the create budget counts over.
pub const METADATA_KEY_MAX_CREATES_WINDOW_SECS: &str = "progression.max_creates_window_secs";

/// A run that ended `no_changes_needed` judged the item not stuck in any way a
/// verb addresses, so the item is re-nominated only after the cooldown times
/// this factor (design 2.5, "`no_action` is not an escalation").
pub const NO_ACTION_COOLDOWN_MULTIPLIER: i64 = 4;

/// Consecutive progression runs ending in infrastructure failure
/// (`failed | orphaned | abandoned`) after which the sweep stops nominating the
/// item (design 2.5, "Infrastructure failure"). This is a protocol constant of
/// the ladder, not a tunable: two is what "twice in a row" means.
pub const INFRASTRUCTURE_FAILURE_CAP: i64 = 2;

/// The limits that bound progression (design section 2.6).
///
/// Defaults are the table's placeholders. See the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, bon::Builder)]
pub struct ProgressionSettings {
    /// `progression.pool_size`: slots in the dedicated progression pool.
    /// Placeholder 1. When the pool is full, candidates wait in the sweep's
    /// queue as `ready` executions, oldest stuck first.
    #[builder(default = DEFAULT_POOL_SIZE)]
    pub pool_size: usize,
    /// `progression.sweep_interval`, in seconds. Placeholder 5 minutes. Nothing
    /// fires at the limit: a candidate waits at most one interval.
    #[builder(default = DEFAULT_SWEEP_INTERVAL_SECS)]
    pub sweep_interval_secs: u64,
    /// `progression.stuck_after`, in seconds: how long an item must have been
    /// continuously stuck before its first nomination. Placeholder 2 hours. An
    /// un-park and a worker-declared blocker waive it once.
    #[builder(default = DEFAULT_STUCK_AFTER_SECS)]
    pub stuck_after_secs: i64,
    /// `progression.cooldown`, in seconds, between progression runs on one
    /// item. Placeholder 6 hours.
    #[builder(default = DEFAULT_COOLDOWN_SECS)]
    pub cooldown_secs: i64,
    /// `progression.max_creates_per_item_window`: revisions plus precursors a
    /// progression run may create for one item per window. Placeholder 3. The
    /// candidate views read it (an exhausted budget stops the sweep nominating
    /// the item); no verb layer enforces it on a running agent yet.
    #[builder(default = DEFAULT_MAX_CREATES_PER_ITEM_WINDOW)]
    pub max_creates_per_item_window: u32,
    /// The rolling window `max_creates_per_item_window` counts over, in
    /// seconds. Placeholder 7 days.
    #[builder(default = DEFAULT_MAX_CREATES_WINDOW_SECS)]
    pub max_creates_window_secs: i64,
    /// `progression.run_timeout`, in seconds. Placeholder 30 minutes. The value is
    /// declared and parsed but nothing enforces it yet: a progression run is
    /// bounded only by the stale-worker reaper's ordinary rules.
    #[builder(default = DEFAULT_RUN_TIMEOUT_SECS)]
    pub run_timeout_secs: u64,
    /// `progression.expected_questions_per_week`: an alarm threshold on the
    /// question rate, never a cap. Unset until measured; nothing is dropped or
    /// delayed either way.
    pub expected_questions_per_week: Option<u32>,
}

/// Placeholder `progression.pool_size`.
pub const DEFAULT_POOL_SIZE: usize = 1;
/// Placeholder `progression.sweep_interval` (5 minutes).
pub const DEFAULT_SWEEP_INTERVAL_SECS: u64 = 300;
/// Placeholder `progression.stuck_after` (2 hours).
pub const DEFAULT_STUCK_AFTER_SECS: i64 = 2 * 60 * 60;
/// Placeholder `progression.cooldown` (6 hours).
pub const DEFAULT_COOLDOWN_SECS: i64 = 6 * 60 * 60;
/// Placeholder `progression.max_creates_per_item_window` (3 per window).
pub const DEFAULT_MAX_CREATES_PER_ITEM_WINDOW: u32 = 3;
/// Placeholder window for `progression.max_creates_per_item_window` (7 days).
pub const DEFAULT_MAX_CREATES_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;
/// Placeholder `progression.run_timeout` (30 minutes).
pub const DEFAULT_RUN_TIMEOUT_SECS: u64 = 30 * 60;

impl Default for ProgressionSettings {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl ProgressionSettings {
    /// The pool size the engine actually builds: the configured value clamped to
    /// the slot-namespace ceiling.
    pub fn effective_pool_size(&self) -> usize {
        self.pool_size.min(crate::pool_policy::MAX_PROGRESSION_POOL_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_design_placeholders() {
        let settings = ProgressionSettings::default();
        assert_eq!(settings.pool_size, 1);
        assert_eq!(settings.sweep_interval_secs, 5 * 60);
        assert_eq!(settings.stuck_after_secs, 2 * 3600);
        assert_eq!(settings.cooldown_secs, 6 * 3600);
        assert_eq!(settings.max_creates_per_item_window, 3);
        assert_eq!(settings.max_creates_window_secs, 7 * 24 * 3600);
        assert_eq!(settings.run_timeout_secs, 30 * 60);
        assert_eq!(settings.expected_questions_per_week, None);
    }

    #[test]
    fn effective_pool_size_never_exceeds_the_slot_namespace() {
        let oversized = ProgressionSettings::builder().pool_size(1_000).build();
        assert_eq!(
            oversized.effective_pool_size(),
            crate::pool_policy::MAX_PROGRESSION_POOL_SIZE
        );
        assert_eq!(ProgressionSettings::default().effective_pool_size(), 1);
    }
}
