//! Retention adapter for recorded Grok run containers.
//!
//! Planning, reclaim, and sizing live in
//! `boss_engine_driver_home_retention`. This crate keeps Grok's policy
//! constants and env names.
//!
//! A Grok per-run container (`boss_engine_driver::grok::home::grok_run_container_for_run`)
//! holds two sibling directories: `grok-home/` (`GROK_HOME`) and
//! `process-home/` (the scoped process `HOME`). Reclaiming a run means
//! removing the whole container so neither half becomes a permanent orphan
//! of the other.
//!
//! Cleanup operates **only** on paths recorded in per-execution
//! driver-owned runtime state (`GrokRuntimeState.grok_home`, via its run
//! container), never inferred from the engine environment. Interactive
//! history under `~/.grok` is never scanned.

pub use boss_engine_driver_home_retention::{
    OwnedHome as OwnedGrokHome, ReclaimPlan, ReclaimReport, RetentionPolicy, directory_size_bytes,
};
use std::path::Path;
use std::time::Duration;

/// Default retention window for terminal homes. Same value as Codex's
/// `codex_rollout_retention::DEFAULT_MAX_AGE_DAYS` so an operator does not
/// need to learn two different retention windows.
pub const DEFAULT_MAX_AGE_DAYS: u64 = 14;

/// Default total-bytes backstop across retained terminal homes. Same value
/// as Codex's `codex_rollout_retention::DEFAULT_MAX_TOTAL_BYTES`.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;

/// Env var overriding [`DEFAULT_MAX_AGE_DAYS`].
pub const MAX_AGE_DAYS_ENV: &str = "BOSS_GROK_HOME_RETENTION_DAYS";
/// Env var overriding [`DEFAULT_MAX_TOTAL_BYTES`].
pub const MAX_TOTAL_BYTES_ENV: &str = "BOSS_GROK_HOME_MAX_BYTES";

/// Operator-tunable retention bound. See [`RetentionPolicy`] for the shared
/// retention rule.
#[derive(Debug, Clone, Copy)]
pub struct GrokHomeRetentionPolicy {
    pub max_age: Duration,
    pub max_total_bytes: u64,
}

impl Default for GrokHomeRetentionPolicy {
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

impl GrokHomeRetentionPolicy {
    /// Construct a policy with explicit knobs (tests / bossctl flags).
    pub fn new(max_age: Duration, max_total_bytes: u64) -> Self {
        Self {
            max_age,
            max_total_bytes,
        }
    }
}

pub fn plan_reclaim(homes: &[OwnedGrokHome], now_epoch: i64, policy: &GrokHomeRetentionPolicy) -> ReclaimPlan {
    boss_engine_driver_home_retention::plan_reclaim(
        homes,
        now_epoch,
        &RetentionPolicy::new(policy.max_age, policy.max_total_bytes),
    )
}

pub fn execute_reclaim(
    homes: &[OwnedGrokHome],
    now_epoch: i64,
    policy: &GrokHomeRetentionPolicy,
    dry_run: bool,
    safety: &dyn Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<ReclaimReport> {
    boss_engine_driver_home_retention::execute_reclaim(
        homes,
        now_epoch,
        &RetentionPolicy::new(policy.max_age, policy.max_total_bytes),
        dry_run,
        safety,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise env-mutating policy tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn default_policy_reads_env_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: serialised by ENV_LOCK.
        unsafe {
            std::env::set_var(MAX_AGE_DAYS_ENV, "3");
            std::env::set_var(MAX_TOTAL_BYTES_ENV, "1024");
        }
        let policy = GrokHomeRetentionPolicy::default();
        assert_eq!(policy.max_age, Duration::from_secs(3 * 24 * 60 * 60));
        assert_eq!(policy.max_total_bytes, 1024);
        unsafe {
            std::env::remove_var(MAX_AGE_DAYS_ENV);
            std::env::remove_var(MAX_TOTAL_BYTES_ENV);
        }
    }
}
