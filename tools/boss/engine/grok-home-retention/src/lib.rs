//! Retention and cleanup for **Boss-owned** per-run Grok home containers.
//!
//! Mirrors `tools/boss/engine/codex-rollout-retention/src/lib.rs` — same
//! rules, same shape, differing only in what a Grok "home" is on disk.
//!
//! A Grok per-run container (`boss_engine_driver::grok::home::grok_run_container_for_run`,
//! `tools/boss/engine/driver/src/grok/home.rs:115`) holds two sibling
//! directories: `grok-home/` (`GROK_HOME`) and `process-home/` (the scoped
//! process `HOME`, T-01). Reclaiming a run means removing the whole
//! container so neither half becomes a permanent orphan of the other.
//!
//! ## Why not scan `~/.grok`
//!
//! Same reasoning as Codex's rollout retention: a worker's per-run home can
//! live outside the operator's interactive home and would be missed, and
//! interactive history under `~/.grok` must never be touched by accident.
//! Cleanup therefore operates **only** on paths recorded in per-execution
//! driver-owned runtime state (`GrokRuntimeState.grok_home`, via its run
//! container), never inferred from the engine environment.
//!
//! ## Retention rule
//!
//! Identical policy to Codex's (`codex_rollout_retention::CodexHomeRetentionPolicy`,
//! `tools/boss/engine/codex-rollout-retention/src/lib.rs:69`): age-based, not
//! count-based. A **terminal** execution's recorded home is eligible for
//! reclaim once its age anchor (`finished_at`, else `created_at`) is older
//! than [`GrokHomeRetentionPolicy::max_age`] (default 14 days — same value
//! as Codex's `DEFAULT_MAX_AGE_DAYS`). A total-bytes backstop
//! ([`GrokHomeRetentionPolicy::max_total_bytes`], default 500 MiB — same
//! value as Codex's `DEFAULT_MAX_TOTAL_BYTES`) bounds growth inside the age
//! window by reclaiming oldest terminal homes first.
//!
//! A home whose owning execution is still **live** (non-terminal) is never
//! reclaimed — liveness is an execution-status fact supplied by the caller,
//! never inferred from file mtime. Liveness is aggregated **per path**: if
//! any candidate for path `P` is live, `P` is never reclaimed even when
//! another row for `P` is terminal.
//!
//! ## Wiring
//!
//! [`plan_reclaim`] is pure and unit-tested. The engine sweep
//! (`grok_home_retention_sweep`, mirroring `codex_home_retention_sweep`)
//! loads recorded roots from `work_executions.driver_runtime_state`,
//! classifies live/terminal from execution status, and calls
//! [`execute_reclaim`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use walkdir::WalkDir;

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

/// Operator-tunable retention bound. See the module doc for the rule.
#[derive(Debug, Clone, Copy)]
pub struct GrokHomeRetentionPolicy {
    pub max_age: Duration,
    pub max_total_bytes: u64,
}

impl Default for GrokHomeRetentionPolicy {
    fn default() -> Self {
        let max_age_days = std::env::var(MAX_AGE_DAYS_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_AGE_DAYS);
        let max_total_bytes = std::env::var(MAX_TOTAL_BYTES_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_TOTAL_BYTES);
        Self {
            max_age: Duration::from_secs(max_age_days.saturating_mul(24 * 60 * 60)),
            max_total_bytes,
        }
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

/// One Boss-owned Grok run-container candidate. Built by the engine from
/// recorded `driver_runtime_state` + execution status — never by scanning a
/// default provider home. `path` is the run **container** (parent of
/// `grok-home/`), not `GROK_HOME` alone, so a reclaim removes `process-home/`
/// along with it.
#[derive(Debug, Clone)]
pub struct OwnedGrokHome {
    pub path: PathBuf,
    pub execution_id: String,
    /// `true` when the owning execution is non-terminal. Live homes are
    /// never reclaimed, regardless of age or the byte cap.
    pub is_live: bool,
    /// Epoch seconds used for age: `finished_at` when present, else
    /// `created_at`. Not file mtime.
    pub age_anchor_epoch: i64,
    /// On-disk size of the container tree (0 if missing / unreadable).
    pub size_bytes: u64,
}

/// Result of [`plan_reclaim`]: which roots to delete plus counters.
#[derive(Debug, Default, Clone)]
pub struct ReclaimPlan {
    pub to_delete: Vec<PathBuf>,
    pub scanned: usize,
    pub skipped_live: usize,
    pub kept_in_policy: usize,
    pub deleted_bytes: u64,
}

/// Result of [`execute_reclaim`]: what actually happened on disk.
#[derive(Debug, Default, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct ReclaimReport {
    #[builder(default)]
    pub scanned: usize,
    #[builder(default)]
    pub deleted: Vec<PathBuf>,
    #[builder(default)]
    pub deleted_bytes: u64,
    #[builder(default)]
    pub skipped_live: usize,
    #[builder(default)]
    pub kept_in_policy: usize,
    #[builder(default)]
    pub dry_run: bool,
    /// Per-path failures. Best-effort: one failure does not abort the rest.
    #[builder(default)]
    pub errors: Vec<String>,
}

/// Decide which recorded homes to reclaim. Pure: no I/O.
///
/// - Live homes are never deleted.
/// - **Path-level live guard:** if *any* candidate with path `P` is live
///   (non-terminal), `P` is never placed in `to_delete` — even when another
///   row for the same path is terminal. Duplicate paths are collapsed so
///   size is not double-counted under the byte cap.
/// - Terminal homes older than `policy.max_age` are deleted.
/// - Remaining terminal homes are deleted oldest-first while their total
///   size exceeds `policy.max_total_bytes`.
pub fn plan_reclaim(homes: &[OwnedGrokHome], now_epoch: i64, policy: &GrokHomeRetentionPolicy) -> ReclaimPlan {
    use std::collections::{HashMap, HashSet};

    // Path-level live aggregation: one live row for P protects every row
    // that records P (re-dispatch / partial-provision edge cases).
    let mut live_paths: HashSet<&Path> = HashSet::new();
    let mut skipped_live = 0usize;
    for home in homes {
        if home.is_live {
            live_paths.insert(home.path.as_path());
            skipped_live += 1;
        }
    }

    // Collapse terminal candidates by path. Skip any path that still has a
    // live owner. When multiple terminal rows share a path (unusual but
    // possible), keep the oldest age anchor and the max reported size so
    // the byte cap is not inflated by duplicate listings of one tree.
    let mut terminal_by_path: HashMap<&Path, (i64, u64)> = HashMap::new();
    for home in homes {
        if live_paths.contains(home.path.as_path()) {
            continue;
        }
        if home.is_live {
            continue;
        }
        terminal_by_path
            .entry(home.path.as_path())
            .and_modify(|(age, size)| {
                *age = (*age).min(home.age_anchor_epoch);
                *size = (*size).max(home.size_bytes);
            })
            .or_insert((home.age_anchor_epoch, home.size_bytes));
    }

    let mut terminal: Vec<(&Path, i64, u64)> = terminal_by_path
        .into_iter()
        .map(|(path, (age, size))| (path, age, size))
        .collect();
    // Oldest age-anchor first so the byte-cap backstop reclaims least-recent
    // terminal evidence first.
    terminal.sort_by_key(|(_, age, _)| *age);

    let max_age_secs = policy.max_age.as_secs() as i64;
    let mut remaining_bytes: u64 = terminal.iter().map(|(_, _, size)| *size).sum();
    let mut to_delete = Vec::new();
    let mut deleted_bytes = 0u64;

    for (path, age_anchor_epoch, size_bytes) in &terminal {
        let age_secs = now_epoch.saturating_sub(*age_anchor_epoch);
        let too_old = age_secs > max_age_secs;
        let over_cap = remaining_bytes > policy.max_total_bytes;
        if too_old || over_cap {
            to_delete.push(path.to_path_buf());
            deleted_bytes = deleted_bytes.saturating_add(*size_bytes);
            remaining_bytes = remaining_bytes.saturating_sub(*size_bytes);
        }
    }

    ReclaimPlan {
        kept_in_policy: terminal.len().saturating_sub(to_delete.len()),
        scanned: homes.len(),
        skipped_live,
        to_delete,
        deleted_bytes,
    }
}

/// Apply a [`ReclaimPlan`] (or recompute from `homes`) and delete roots.
///
/// `safety` is called before each delete: the engine passes
/// `boss_engine_driver::grok::assert_grok_home_safe_to_delete` so a tampered
/// path cannot escape the Boss homes root. Missing paths are treated as
/// already-reclaimed (idempotent). Per-path errors are collected; the
/// function itself only fails on a pre-plan hard error (none today).
///
/// Deletion never follows symlinks: `std::fs::remove_dir_all` unlinks a
/// symlink entry it encounters rather than descending into (or deleting)
/// its target, so the `auth.json` credential symlink inside a reclaimed
/// `GROK_HOME` is removed while the real credential file it points at
/// (outside the homes root) is untouched.
pub fn execute_reclaim(
    homes: &[OwnedGrokHome],
    now_epoch: i64,
    policy: &GrokHomeRetentionPolicy,
    dry_run: bool,
    safety: &dyn Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<ReclaimReport> {
    let plan = plan_reclaim(homes, now_epoch, policy);
    let mut deleted = Vec::new();
    let mut errors = Vec::new();

    if dry_run {
        deleted = plan.to_delete.clone();
    } else {
        for path in &plan.to_delete {
            if let Err(err) = safety(path) {
                errors.push(format!("{}: safety refused: {err:#}", path.display()));
                continue;
            }
            if !path.exists() {
                // Already gone — idempotent success.
                deleted.push(path.clone());
                continue;
            }
            match std::fs::remove_dir_all(path) {
                Ok(()) => deleted.push(path.clone()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    deleted.push(path.clone());
                }
                Err(err) => errors.push(format!("{}: {err}", path.display())),
            }
        }
    }

    Ok(ReclaimReport {
        scanned: plan.scanned,
        deleted,
        deleted_bytes: plan.deleted_bytes,
        skipped_live: plan.skipped_live,
        kept_in_policy: plan.kept_in_policy,
        dry_run,
        errors,
    })
}

/// Best-effort recursive size of a directory tree. Returns 0 when the path
/// is missing or unreadable (size is only a backstop, never a hard gate).
/// Does not follow symlinked entries into their targets for size accounting
/// (`WalkDir`'s default `follow_links(false)`), matching the deletion
/// contract above.
pub fn directory_size_bytes(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise env-mutating policy tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn home(path: &str, exec: &str, live: bool, age_anchor: i64, size: u64) -> OwnedGrokHome {
        OwnedGrokHome {
            path: PathBuf::from(path),
            execution_id: exec.into(),
            is_live: live,
            age_anchor_epoch: age_anchor,
            size_bytes: size,
        }
    }

    const DAY: i64 = 24 * 60 * 60;

    #[test]
    fn live_homes_are_never_reclaimed_regardless_of_age_or_cap() {
        let now = 1_800_000_000i64;
        let homes = vec![home("/tmp/boss-grok-homes/live", "e1", true, now - 400 * DAY, 10_000)];
        let plan = plan_reclaim(&homes, now, &GrokHomeRetentionPolicy::new(Duration::from_secs(1), 0));
        assert!(plan.to_delete.is_empty(), "live home must never be reclaimed");
        assert_eq!(plan.skipped_live, 1);
        assert_eq!(plan.kept_in_policy, 0);
    }

    #[test]
    fn terminal_home_older_than_max_age_is_reclaimed() {
        let now = 1_800_000_000i64;
        let homes = vec![home("/tmp/boss-grok-homes/old", "e1", false, now - 20 * DAY, 1_000)];
        let plan = plan_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(14 * DAY as u64), u64::MAX),
        );
        assert_eq!(plan.to_delete, vec![PathBuf::from("/tmp/boss-grok-homes/old")]);
        assert_eq!(plan.deleted_bytes, 1_000);
        assert_eq!(plan.kept_in_policy, 0);
    }

    #[test]
    fn terminal_home_within_max_age_is_kept() {
        let now = 1_800_000_000i64;
        let homes = vec![home("/tmp/boss-grok-homes/recent", "e1", false, now - DAY, 1_000)];
        let plan = plan_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(14 * DAY as u64), u64::MAX),
        );
        assert!(plan.to_delete.is_empty());
        assert_eq!(plan.kept_in_policy, 1);
    }

    #[test]
    fn byte_cap_evicts_oldest_terminal_first_even_within_age_window() {
        let now = 1_800_000_000i64;
        let homes = vec![
            home("/tmp/h/oldest", "e1", false, now - 3 * DAY, 1_000),
            home("/tmp/h/middle", "e2", false, now - 2 * DAY, 1_000),
            home("/tmp/h/newest", "e3", false, now - DAY, 1_000),
        ];
        let plan = plan_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(14 * DAY as u64), 1_500),
        );
        assert_eq!(
            plan.to_delete,
            vec![PathBuf::from("/tmp/h/oldest"), PathBuf::from("/tmp/h/middle")]
        );
        assert_eq!(plan.kept_in_policy, 1);
    }

    #[test]
    fn byte_cap_never_evicts_a_live_home() {
        let now = 1_800_000_000i64;
        let homes = vec![
            home("/tmp/h/live", "e1", true, now - 3 * DAY, 10_000),
            home("/tmp/h/dead", "e2", false, now - DAY, 1),
        ];
        let plan = plan_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(14 * DAY as u64), 0),
        );
        assert_eq!(plan.to_delete, vec![PathBuf::from("/tmp/h/dead")]);
        assert_eq!(plan.skipped_live, 1);
    }

    #[test]
    fn path_level_live_guard_blocks_delete_when_any_row_for_path_is_live() {
        // Same path P recorded on a live execution and an old terminal
        // sibling (re-dispatch / partial-provision). The terminal row must
        // not cause P to be reclaimed while any live owner remains.
        let now = 1_800_000_000i64;
        let path = "/tmp/boss-grok-homes/shared";
        let homes = vec![
            home(path, "e-terminal-old", false, now - 40 * DAY, 5_000),
            home(path, "e-live", true, now - DAY, 5_000),
        ];
        let plan = plan_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(14 * DAY as u64), 0),
        );
        assert!(
            plan.to_delete.is_empty(),
            "live path must never enter to_delete even when a terminal sibling is eligible: {plan:?}"
        );
        assert_eq!(plan.skipped_live, 1);
        assert_eq!(plan.kept_in_policy, 0);
    }

    #[test]
    fn duplicate_terminal_paths_are_collapsed_not_double_counted() {
        let now = 1_800_000_000i64;
        let path = "/tmp/boss-grok-homes/dup";
        let homes = vec![
            home(path, "e1", false, now - 3 * DAY, 1_000),
            home(path, "e2", false, now - DAY, 1_000),
        ];
        let plan = plan_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(14 * DAY as u64), 1_500),
        );
        assert!(
            plan.to_delete.is_empty(),
            "collapsed path size 1000 is under cap 1500: {plan:?}"
        );
        assert_eq!(plan.kept_in_policy, 1);
        assert_eq!(plan.scanned, 2);

        let plan_age = plan_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(1), u64::MAX),
        );
        assert_eq!(plan_age.to_delete, vec![PathBuf::from(path)]);
        assert_eq!(plan_age.deleted_bytes, 1_000);
    }

    #[test]
    fn only_recorded_paths_are_candidates_no_cwd_inference() {
        let plan = plan_reclaim(
            &[],
            1_800_000_000,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(1), 0),
        );
        assert!(plan.to_delete.is_empty());
        assert_eq!(plan.scanned, 0);
    }

    #[test]
    fn execute_reclaim_is_idempotent_when_path_already_gone() {
        let now = 1_800_000_000i64;
        let missing = PathBuf::from("/tmp/boss-grok-homes/already-gone-does-not-exist");
        let homes = vec![OwnedGrokHome {
            path: missing.clone(),
            execution_id: "e1".into(),
            is_live: false,
            age_anchor_epoch: now - 40 * DAY,
            size_bytes: 0,
        }];
        let report = execute_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(1), u64::MAX),
            false,
            &|_| Ok(()),
        )
        .unwrap();
        assert_eq!(report.deleted, vec![missing]);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn execute_reclaim_dry_run_does_not_delete() {
        let dir = tempfile::tempdir().unwrap();
        let container = dir.path().join("run-1");
        std::fs::create_dir_all(container.join("grok-home")).unwrap();
        std::fs::create_dir_all(container.join("process-home")).unwrap();
        std::fs::write(container.join("grok-home/marker"), "keep").unwrap();

        let now = 1_800_000_000i64;
        let homes = vec![OwnedGrokHome {
            path: container.clone(),
            execution_id: "e1".into(),
            is_live: false,
            age_anchor_epoch: now - 40 * DAY,
            size_bytes: 10,
        }];
        let report = execute_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(1), u64::MAX),
            true,
            &|_| Ok(()),
        )
        .unwrap();
        assert_eq!(report.deleted, vec![container.clone()]);
        assert!(report.dry_run);
        assert!(container.join("grok-home/marker").is_file(), "dry run must not delete");
    }

    #[test]
    fn execute_reclaim_deletes_recorded_root_and_honours_safety() {
        let dir = tempfile::tempdir().unwrap();
        let container = dir.path().join("run-1");
        std::fs::create_dir_all(container.join("grok-home/sessions")).unwrap();
        std::fs::create_dir_all(container.join("process-home")).unwrap();
        std::fs::write(container.join("grok-home/sessions/updates.jsonl"), "{}\n").unwrap();

        let now = 1_800_000_000i64;
        let homes = vec![OwnedGrokHome {
            path: container.clone(),
            execution_id: "e1".into(),
            is_live: false,
            age_anchor_epoch: now - 40 * DAY,
            size_bytes: 100,
        }];
        let report = execute_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(1), u64::MAX),
            false,
            &|_| Ok(()),
        )
        .unwrap();
        assert_eq!(report.deleted, vec![container.clone()]);
        assert!(!container.exists());

        // Safety refusal leaves the path alone.
        let container2 = dir.path().join("run-2");
        std::fs::create_dir_all(&container2).unwrap();
        let homes2 = vec![OwnedGrokHome {
            path: container2.clone(),
            execution_id: "e2".into(),
            is_live: false,
            age_anchor_epoch: now - 40 * DAY,
            size_bytes: 1,
        }];
        let report2 = execute_reclaim(
            &homes2,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(1), u64::MAX),
            false,
            &|_| Err(anyhow::anyhow!("outside homes root")),
        )
        .unwrap();
        assert!(report2.deleted.is_empty());
        assert_eq!(report2.errors.len(), 1);
        assert!(container2.is_dir());
    }

    #[test]
    fn execute_reclaim_removes_symlink_entry_but_never_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let container = dir.path().join("run-1");
        let grok_home = container.join("grok-home");
        std::fs::create_dir_all(&grok_home).unwrap();
        std::fs::create_dir_all(container.join("process-home")).unwrap();

        // Real credential file living entirely outside the homes root.
        let real_auth = dir.path().join("real-auth.json");
        std::fs::write(&real_auth, "super-secret-token").unwrap();
        // Symlink inside GROK_HOME pointing at the real credential, exactly
        // as Grok home provisioning wires it.
        std::os::unix::fs::symlink(&real_auth, grok_home.join("auth.json")).unwrap();

        let now = 1_800_000_000i64;
        let homes = vec![OwnedGrokHome {
            path: container.clone(),
            execution_id: "e1".into(),
            is_live: false,
            age_anchor_epoch: now - 40 * DAY,
            size_bytes: 0,
        }];
        let report = execute_reclaim(
            &homes,
            now,
            &GrokHomeRetentionPolicy::new(Duration::from_secs(1), u64::MAX),
            false,
            &|_| Ok(()),
        )
        .unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        assert!(
            !container.exists(),
            "run container (including the symlink) must be gone"
        );
        assert!(real_auth.exists(), "symlink target outside the home must survive");
        assert_eq!(std::fs::read_to_string(&real_auth).unwrap(), "super-secret-token");
    }

    #[test]
    fn directory_size_bytes_sums_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/f"), "12345").unwrap();
        std::fs::write(dir.path().join("b"), "xx").unwrap();
        assert_eq!(directory_size_bytes(dir.path()), 7);
        assert_eq!(directory_size_bytes(&dir.path().join("missing")), 0);
    }

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
