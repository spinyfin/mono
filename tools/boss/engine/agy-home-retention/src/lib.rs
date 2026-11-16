//! Retention adapter for recorded Antigravity run containers.
//! Reclaims the parent of process-home, including control and rules directories.
//! Interactive ~/.gemini is never scanned.
pub use boss_engine_driver_home_retention::{
    OwnedHome as OwnedAgyHome, ReclaimPlan, ReclaimReport, RetentionPolicy, directory_size_bytes,
};
use std::path::Path;
use std::time::Duration;
/// Uses the same retention window as Grok.
pub const DEFAULT_MAX_AGE_DAYS: u64 = 14;

/// Default total-bytes backstop across retained terminal homes. Same value
/// as Grok's `grok_home_retention::DEFAULT_MAX_TOTAL_BYTES`.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;

/// Env var overriding [`DEFAULT_MAX_AGE_DAYS`].
pub const MAX_AGE_DAYS_ENV: &str = "BOSS_AGY_HOME_RETENTION_DAYS";
/// Env var overriding [`DEFAULT_MAX_TOTAL_BYTES`].
pub const MAX_TOTAL_BYTES_ENV: &str = "BOSS_AGY_HOME_MAX_BYTES";

/// Tunable retention bound. See [`RetentionPolicy`] for the shared retention rule.
#[derive(Debug, Clone, Copy)]
pub struct AgyHomeRetentionPolicy {
    pub max_age: Duration,
    pub max_total_bytes: u64,
}

impl Default for AgyHomeRetentionPolicy {
    fn default() -> Self {
        let p = RetentionPolicy::from_env(
            MAX_AGE_DAYS_ENV,
            MAX_TOTAL_BYTES_ENV,
            DEFAULT_MAX_AGE_DAYS,
            DEFAULT_MAX_TOTAL_BYTES,
        );
        Self::new(p.max_age, p.max_total_bytes)
    }
}

impl AgyHomeRetentionPolicy {
    /// Construct a policy with explicit knobs (tests / bossctl flags).
    pub fn new(max_age: Duration, max_total_bytes: u64) -> Self {
        Self {
            max_age,
            max_total_bytes,
        }
    }
}

pub fn plan_reclaim(homes: &[OwnedAgyHome], now: i64, policy: &AgyHomeRetentionPolicy) -> ReclaimPlan {
    boss_engine_driver_home_retention::plan_reclaim(
        homes,
        now,
        &RetentionPolicy::new(policy.max_age, policy.max_total_bytes),
    )
}
pub fn execute_reclaim(
    homes: &[OwnedAgyHome],
    now: i64,
    policy: &AgyHomeRetentionPolicy,
    dry_run: bool,
    safety: &dyn Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<ReclaimReport> {
    boss_engine_driver_home_retention::execute_reclaim(
        homes,
        now,
        &RetentionPolicy::new(policy.max_age, policy.max_total_bytes),
        dry_run,
        safety,
    )
}
