//! Retention adapter for recorded Antigravity run containers.
//! Reclaims the parent of process-home, including control and rules directories.
//! Containers holding transcript evidence are retained regardless of age or byte cap.
//! Interactive ~/.gemini is never scanned.
pub use boss_engine_driver_home_retention::{OwnedHome as OwnedAgyHome, RetentionPolicy, directory_size_bytes};
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

/// Shared reclaim accounting and transcript-preservation accounting.
#[derive(Debug)]
pub struct TranscriptRetention<T> {
    inner: T,
    /// Distinct terminal containers retained outside the reclaimable byte allowance.
    pub kept_for_transcripts: usize,
    /// Sum of recorded container sizes, deduplicated by path using the largest size.
    pub kept_for_transcripts_bytes: u64,
}
impl<T> std::ops::Deref for TranscriptRetention<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}
pub type ReclaimPlan = TranscriptRetention<boss_engine_driver_home_retention::ReclaimPlan>;
pub type ReclaimReport = TranscriptRetention<boss_engine_driver_home_retention::ReclaimReport>;

/// Plan retention using the driver-owned transcript protection predicate.
pub fn plan_reclaim(
    homes: &[OwnedAgyHome],
    now: i64,
    policy: &AgyHomeRetentionPolicy,
    probe: &dyn Fn(&Path) -> bool,
) -> ReclaimPlan {
    let (eligible, protected) = partition_transcript_homes(homes, probe);
    let mut plan = boss_engine_driver_home_retention::plan_reclaim(
        &eligible,
        now,
        &RetentionPolicy::new(policy.max_age, policy.max_total_bytes),
    );
    plan.scanned += homes.len() - eligible.len();
    wrap(plan, &protected)
}
/// Apply retention using the same driver-owned protection predicate as planning.
pub fn execute_reclaim(
    homes: &[OwnedAgyHome],
    now: i64,
    policy: &AgyHomeRetentionPolicy,
    dry_run: bool,
    safety: &dyn Fn(&Path) -> anyhow::Result<()>,
    probe: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<ReclaimReport> {
    let (eligible, protected) = partition_transcript_homes(homes, probe);
    let mut report = boss_engine_driver_home_retention::execute_reclaim(
        &eligible,
        now,
        &RetentionPolicy::new(policy.max_age, policy.max_total_bytes),
        dry_run,
        safety,
    )?;
    report.scanned += homes.len() - eligible.len();
    Ok(wrap(report, &protected))
}
fn wrap<T>(inner: T, protected: &[OwnedAgyHome]) -> TranscriptRetention<T> {
    TranscriptRetention {
        inner,
        kept_for_transcripts: protected.len(),
        kept_for_transcripts_bytes: protected
            .iter()
            .fold(0u64, |sum, home| sum.saturating_add(home.size_bytes)),
    }
}
fn partition_transcript_homes(
    homes: &[OwnedAgyHome],
    probe: &dyn Fn(&Path) -> bool,
) -> (Vec<OwnedAgyHome>, Vec<OwnedAgyHome>) {
    let mut paths = std::collections::BTreeMap::<_, Vec<&OwnedAgyHome>>::new();
    for home in homes {
        paths.entry(&home.path).or_default().push(home);
    }
    let mut eligible = Vec::new();
    let mut protected = Vec::new();
    for (path, owners) in paths {
        // Keep all owners visible to the shared live guard and probe each
        // fully-terminal path only once, even during concurrent transcript writes.
        if owners.iter().any(|home| home.is_live) || !probe(path) {
            eligible.extend(owners.into_iter().cloned());
        } else {
            let mut home = owners[0].clone();
            home.size_bytes = owners.iter().map(|home| home.size_bytes).max().unwrap_or(0);
            protected.push(home);
        }
    }
    (eligible, protected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn changing_transcripts_cannot_split_live_and_terminal_owners() {
        for live_first in [false, true] {
            let home = OwnedAgyHome {
                path: "/unused/live-container".into(),
                execution_id: "terminal".into(),
                is_live: false,
                age_anchor_epoch: 0,
                size_bytes: 42,
            };
            let mut live = home.clone();
            live.is_live = true;
            live.execution_id = "live".into();
            let homes = if live_first { vec![live, home] } else { vec![home, live] };
            for policy in [
                AgyHomeRetentionPolicy::new(Duration::ZERO, u64::MAX),
                AgyHomeRetentionPolicy::new(Duration::MAX, 0),
            ] {
                let calls = Cell::new(0);
                let probe = |_: &Path| {
                    let previous = calls.replace(calls.get() + 1);
                    previous > 0
                };
                let plan = plan_reclaim(&homes, 100, &policy, &probe);
                assert!(plan.to_delete.is_empty());
                assert_eq!(plan.skipped_live, 1);
                for dry_run in [true, false] {
                    let report = execute_reclaim(
                        &homes,
                        100,
                        &policy,
                        dry_run,
                        &|_| panic!("live path reached deletion"),
                        &probe,
                    )
                    .unwrap();
                    assert!(report.deleted.is_empty());
                    assert_eq!(report.skipped_live, 1);
                }
                assert_eq!(
                    calls.get(),
                    0,
                    "live paths must never be partitioned by transcript presence"
                );
            }
        }
    }

    #[test]
    fn duplicate_terminal_paths_are_probed_and_counted_once() {
        let home = OwnedAgyHome {
            path: "/unused/terminal-container".into(),
            execution_id: "terminal".into(),
            is_live: false,
            age_anchor_epoch: 0,
            size_bytes: 42,
        };
        let homes = [home.clone(), home];
        let calls = Cell::new(0);
        let probe = |_: &Path| {
            let previous = calls.replace(calls.get() + 1);
            previous == 0
        };
        let policy = AgyHomeRetentionPolicy::new(Duration::ZERO, 0);
        let plan = plan_reclaim(&homes, 100, &policy, &probe);
        assert_eq!(calls.get(), 1);
        assert!(plan.to_delete.is_empty());
        assert_eq!(plan.scanned, 2);
        assert_eq!(plan.kept_in_policy, 0);
        assert_eq!(plan.kept_for_transcripts, 1);
        assert_eq!(plan.kept_for_transcripts_bytes, 42);
        calls.set(0);
        let report = execute_reclaim(
            &homes,
            100,
            &policy,
            false,
            &|_| panic!("protected path reached deletion"),
            &probe,
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(report.deleted.is_empty());
        assert_eq!(report.kept_for_transcripts, 1);
        assert_eq!(report.kept_for_transcripts_bytes, 42);
    }
}
