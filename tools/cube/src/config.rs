use std::path::PathBuf;

use serde::Deserialize;

use crate::app::CubeError;

pub fn config_dir() -> Result<PathBuf, CubeError> {
    if let Some(path) = std::env::var_os("CUBE_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("cube"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| CubeError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set")))?;
    Ok(PathBuf::from(home).join(".config").join("cube"))
}

pub fn config_file_path() -> Result<PathBuf, CubeError> {
    Ok(config_dir()?.join("cube.toml"))
}

/// A user-configured rule that turns a bare `<reponame>` into a clone URL
/// (and optionally a bespoke clone command). Resolvers keep cube ignorant of
/// any particular hosting setup: LinkedIn's `mint`, a corporate GitHub org,
/// etc. all live in the user's config rather than the cube binary.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoResolver {
    /// Human label, surfaced in errors and `cube repo` provenance.
    pub name: String,
    /// Origin URL template. `{name}` is replaced with the resolved
    /// `<reponame>`; the result is recorded as the repo's origin.
    pub origin_pattern: String,
    /// Optional clone command template. When present, the `{name}`-substituted
    /// string is run (in the workspace pool root) in place of `jj git clone`.
    #[serde(default)]
    pub clone_command: Option<String>,
}

impl RepoResolver {
    /// Substitute `{name}` into `origin_pattern`. Returns `None` when the
    /// pattern would yield an empty string (a misconfigured resolver), so the
    /// caller can keep walking the chain.
    pub fn resolve_origin(&self, name: &str) -> Option<String> {
        let url = self.origin_pattern.replace("{name}", name);
        if url.trim().is_empty() { None } else { Some(url) }
    }

    /// The `{name}`-substituted clone command, if this resolver declares one.
    pub fn resolve_clone_command(&self, name: &str) -> Option<String> {
        self.clone_command.as_ref().map(|cmd| cmd.replace("{name}", name))
    }
}

/// Default retention window for a free workspace that is being withheld from
/// the pool because it is unhealthy (dirty, conflicted, or quarantined),
/// before pool GC salvages its work and reclaims it.
///
/// **24 hours, down from 5 days.** The old threshold provably never fired:
/// across a pool of 106 withheld workspaces the oldest had been retained 4.2
/// days (p50 1.3 days), so retention was in practice unbounded while the
/// effective free pool collapsed from 173 to 3 and dispatch started timing
/// out. A threshold that the steady state never reaches is not a threshold.
///
/// Why 24 hours specifically:
///
/// - It matches cube's own lease TTL (`DEFAULT_LEASE_TTL_SECS`, 24h). A
///   holder that has not come back within a day is already presumed dead by
///   cube's clock; retention of that holder's leftovers should not outlive
///   the lease that produced them.
/// - The pool degrades from healthy to all-dirty within hours at the observed
///   dispatch rate (259 leases per audit window, 29 of the last 60 minting
///   fresh workspaces), so anything measured in days is far outside the loop
///   it needs to close.
/// - Expiry is non-destructive: the work is salvaged to a durable record
///   first (see [`crate::app`]'s salvage module), so a shorter window costs
///   findability, not data.
/// - It is the human window too. Someone who has not gone back to yesterday's
///   crashed worker by the next day re-runs the task; they do not resume the
///   tree.
///
/// Override with `[unhealthy-gc] max-age-hours` (or `max-age-days`) in
/// `cube.toml`, or the `CUBE_UNHEALTHY_GC_MAX_AGE_HOURS` /
/// `CUBE_UNHEALTHY_GC_MAX_AGE_DAYS` environment variables.
pub const DEFAULT_UNHEALTHY_GC_MAX_AGE_HOURS: u64 = 24;

const SECS_PER_HOUR: i64 = 3_600;
const HOURS_PER_DAY: u64 = 24;

/// Controls the time-bounded reclaim of free workspaces that have been
/// continuously unhealthy (dirty, conflicted, or quarantined) for too long.
///
/// All unhealthy states share the same threshold. The struct is designed so a
/// separate, more-aggressive threshold for one of them can be added as a new
/// optional field later without a schema/redesign.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UnhealthyGcConfig {
    /// How many hours a free workspace can be retained while unhealthy before
    /// pool GC salvages and resets it. Takes precedence over
    /// [`Self::max_age_days`]. Defaults to
    /// [`DEFAULT_UNHEALTHY_GC_MAX_AGE_HOURS`].
    #[serde(rename = "max-age-hours")]
    pub max_age_hours: Option<u64>,
    /// Day-granularity form of [`Self::max_age_hours`], kept so existing
    /// `cube.toml` files keep working. Retention is now tuned in hours, so
    /// this is the coarse alias rather than the primary knob.
    #[serde(rename = "max-age-days")]
    pub max_age_days: Option<u64>,
}

impl UnhealthyGcConfig {
    /// Returns the configured retention window in seconds.
    ///
    /// Precedence, most specific first: `CUBE_UNHEALTHY_GC_MAX_AGE_HOURS`,
    /// `CUBE_UNHEALTHY_GC_MAX_AGE_DAYS`, `max-age-hours`, `max-age-days`,
    /// then [`DEFAULT_UNHEALTHY_GC_MAX_AGE_HOURS`].
    pub fn max_age_secs(&self) -> i64 {
        let env_u64 = |key: &str| std::env::var(key).ok().and_then(|v| v.parse::<u64>().ok());
        let hours = env_u64("CUBE_UNHEALTHY_GC_MAX_AGE_HOURS")
            .or_else(|| env_u64("CUBE_UNHEALTHY_GC_MAX_AGE_DAYS").map(|d| d.saturating_mul(HOURS_PER_DAY)))
            .or(self.max_age_hours)
            .or_else(|| self.max_age_days.map(|d| d.saturating_mul(HOURS_PER_DAY)))
            .unwrap_or(DEFAULT_UNHEALTHY_GC_MAX_AGE_HOURS);
        (hours as i64).saturating_mul(SECS_PER_HOUR)
    }
}

/// Default per-repo high-water mark for *free* workspaces.
///
/// The pool has no cap on total workspaces and never will — a lease for a
/// reachable repo always succeeds, and admission control is deliberately not
/// how cube bounds disk (see [`PoolConfig`]). This mark is a GC target, not a
/// gate: when a repo holds more free workspaces than this, pool GC trims the
/// surplus back down to it. Leased workspaces do not count toward it and are
/// never touched by it.
///
/// Twenty is sized off the observed steady state rather than the pathological
/// one. The pool that triggered this work had grown to 520 mono workspaces,
/// but the manual cleanup that recovered the machine left 51 across *three*
/// repos and dispatch continued to work; peak concurrent leases observed was
/// 35, of which 30 were orphans with no live agent. Twenty free per repo is
/// therefore several times the warm capacity real dispatch draws on, while
/// making a 500-entry pool structurally impossible.
pub const DEFAULT_MAX_FREE_WORKSPACES: usize = 20;

/// How long a workspace must have been idle before the *routine* pool GC pass
/// will reclaim its build-artifact trees.
///
/// Compaction costs the next lease a cold build, so a workspace released
/// minutes ago — and likely to be re-leased within the hour — keeps its cache.
/// Six hours is well past the point where an incremental cache is still worth
/// much (`main` has moved, dependencies with it) and well short of the 24h
/// retention TTL. The urgent, disk-pressure pass ignores this window entirely:
/// when the volume is near exhaustion a cold build is unambiguously cheaper
/// than a full disk.
pub const DEFAULT_COMPACT_IDLE_HOURS: u64 = 6;

/// Absolute free-space floor below which a lease triggers reclamation.
///
/// 20 GiB is roughly one fully-built mono workspace (the largest observed
/// `target/` was 20.93 GiB), so this is "less than one more worker's worth of
/// headroom left".
pub const DEFAULT_MIN_FREE_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Proportional free-space floor, as a percentage of the volume's total size.
///
/// Applied as `max(min_free_bytes, min_free_percent% of total)` so a large
/// volume gets proportionally more headroom than a flat byte floor would give
/// it: on the 1.8 TiB volume that filled to exhaustion, 2% is ~36 GiB.
pub const DEFAULT_MIN_FREE_PERCENT: u64 = 2;

/// Directory names, relative to a workspace root, treated as regenerable
/// build output and reclaimed by compaction.
///
/// `target` is Cargo's; `.build` is SwiftPM's. Both are pure compiler output:
/// deleting one costs a rebuild and nothing else, and neither is coupled to
/// any state cube records.
///
/// `node_modules` is deliberately NOT here, though it is the obvious third
/// candidate. It is an *installed dependency tree*, not build output: it needs
/// the network to reconstruct, and cube's own `workspace_setup` table records
/// install steps as completed, so removing it out from under that state leaves
/// a workspace whose setup believes it already ran. It remains available via
/// `[pool] build-artifact-dirs` for anyone who wants it.
///
/// Bazel's outputs need no entry at all: `bazel-out` and friends are symlinks
/// into a single shared output base under `~/Library/Caches/bazel/`, so they
/// occupy no meaningful space in the workspace tree — and compaction refuses
/// to follow symlinks precisely so it can never delete through one into that
/// shared cache.
pub fn default_build_artifact_dirs() -> Vec<String> {
    vec!["target".to_string(), ".build".to_string()]
}

/// Per-repo overrides for the free-workspace high-water mark.
///
/// mono, flunge and checkleft-sandbox have very different footprints (a built
/// mono workspace is tens of GiB; checkleft-sandbox's whole shared store is
/// 0.01 GiB), so the mark is per repo rather than one number for all of them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RepoPoolConfig {
    /// Overrides [`PoolConfig::max_free_workspaces`] for this repo.
    #[serde(rename = "max-free-workspaces")]
    pub max_free_workspaces: Option<usize>,
}

/// Controls how pool GC bounds workspace disk usage.
///
/// Read this together with the deliberate absence of a cap on the *total*
/// number of workspaces. cube grows the pool on demand and a lease for a
/// reachable repo always succeeds; that is a design decision, not an
/// oversight, and it stands. What was missing was the other half — nothing
/// ever gave disk back. Every knob here is about reclamation, and the only
/// one that can fail a call ([`Self::min_free_bytes`]) keys off the volume's
/// actual free space, never off how many workspaces exist.
#[derive(Debug, Clone, Default, Deserialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(default)]
pub struct PoolConfig {
    /// Default per-repo high-water mark for free workspaces. See
    /// [`DEFAULT_MAX_FREE_WORKSPACES`].
    #[serde(rename = "max-free-workspaces")]
    pub max_free_workspaces: Option<usize>,
    /// Per-repo overrides, keyed by repo id (`[pool.repos.mono]`).
    #[builder(default)]
    pub repos: std::collections::HashMap<String, RepoPoolConfig>,
    /// Directory names reclaimed by compaction. See
    /// [`default_build_artifact_dirs`].
    #[serde(rename = "build-artifact-dirs")]
    pub build_artifact_dirs: Option<Vec<String>>,
    /// Idle window before routine compaction touches a workspace. See
    /// [`DEFAULT_COMPACT_IDLE_HOURS`].
    #[serde(rename = "compact-idle-hours")]
    pub compact_idle_hours: Option<u64>,
    /// Absolute free-space floor. See [`DEFAULT_MIN_FREE_BYTES`].
    #[serde(rename = "min-free-bytes")]
    pub min_free_bytes: Option<u64>,
    /// Proportional free-space floor. See [`DEFAULT_MIN_FREE_PERCENT`].
    #[serde(rename = "min-free-percent")]
    pub min_free_percent: Option<u64>,
}

impl PoolConfig {
    /// The free-workspace high-water mark for `repo`: its own override, else
    /// the configured default, else [`DEFAULT_MAX_FREE_WORKSPACES`].
    ///
    /// `CUBE_POOL_MAX_FREE_WORKSPACES` overrides the *default* only, so an
    /// explicit per-repo entry still wins — the env var is an operator's
    /// blunt instrument, not a way to silently retune one repo.
    pub fn max_free_workspaces(&self, repo: &str) -> usize {
        if let Some(per_repo) = self.repos.get(repo).and_then(|r| r.max_free_workspaces) {
            return per_repo;
        }
        env_u64("CUBE_POOL_MAX_FREE_WORKSPACES")
            .map(|v| v as usize)
            .or(self.max_free_workspaces)
            .unwrap_or(DEFAULT_MAX_FREE_WORKSPACES)
    }

    /// Directory names compaction reclaims, relative to a workspace root.
    pub fn build_artifact_dirs(&self) -> Vec<String> {
        self.build_artifact_dirs
            .clone()
            .unwrap_or_else(default_build_artifact_dirs)
    }

    /// Idle window, in seconds, before routine compaction touches a workspace.
    pub fn compact_idle_secs(&self) -> i64 {
        let hours = env_u64("CUBE_POOL_COMPACT_IDLE_HOURS")
            .or(self.compact_idle_hours)
            .unwrap_or(DEFAULT_COMPACT_IDLE_HOURS);
        (hours as i64).saturating_mul(SECS_PER_HOUR)
    }

    /// The free-space floor for a volume of `total_bytes`:
    /// `max(min_free_bytes, min_free_percent% of total)`.
    ///
    /// Below this, a lease reclaims before it does anything else; a mint that
    /// still cannot clear it afterwards fails loudly rather than pushing the
    /// volume over.
    pub fn free_space_floor_bytes(&self, total_bytes: u64) -> u64 {
        let absolute = env_u64("CUBE_POOL_MIN_FREE_BYTES")
            .or(self.min_free_bytes)
            .unwrap_or(DEFAULT_MIN_FREE_BYTES);
        let percent = env_u64("CUBE_POOL_MIN_FREE_PERCENT")
            .or(self.min_free_percent)
            .unwrap_or(DEFAULT_MIN_FREE_PERCENT);
        let proportional = total_bytes / 100 * percent.min(100);
        absolute.max(proportional)
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.parse::<u64>().ok())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CubeConfig {
    /// Ordered list of repo-name resolvers. The first resolver that produces a
    /// URL wins (see `cube repo ensure`).
    #[serde(rename = "repo-resolvers")]
    pub repo_resolvers: Vec<RepoResolver>,
    /// Controls time-bounded reclaim of long-lived unhealthy free workspaces.
    #[serde(rename = "unhealthy-gc")]
    pub unhealthy_gc: UnhealthyGcConfig,
    /// Controls how pool GC bounds workspace disk usage.
    pub pool: PoolConfig,
}

/// Load cube user config from the standard config file path.
/// Returns a default (all-off) config if the file does not exist or the home
/// directory cannot be determined.
pub fn load_config() -> Result<CubeConfig, CubeError> {
    let path = match config_file_path() {
        Ok(p) => p,
        // If we can't determine where config lives (e.g. HOME unset), treat it
        // as absent and return defaults rather than propagating a hard error.
        Err(_) => return Ok(CubeConfig::default()),
    };
    if !path.exists() {
        return Ok(CubeConfig::default());
    }
    let content = std::fs::read_to_string(&path).map_err(CubeError::Io)?;
    toml::from_str(&content)
        .map_err(|e| CubeError::InvalidArgument(format!("failed to parse cube config at {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_resolvers() {
        let cfg = CubeConfig::default();
        assert!(cfg.repo_resolvers.is_empty());
    }

    #[test]
    fn parse_resolver_with_clone_command() {
        let toml = "[[repo-resolvers]]\n\
            name = \"mint\"\n\
            origin_pattern = \"org-127256988@github.com:linkedin-multiproduct/{name}.git\"\n\
            clone_command = \"mint clone {name}\"\n";
        let cfg: CubeConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.repo_resolvers.len(), 1);
        let r = &cfg.repo_resolvers[0];
        assert_eq!(r.name, "mint");
        assert_eq!(
            r.origin_pattern,
            "org-127256988@github.com:linkedin-multiproduct/{name}.git"
        );
        assert_eq!(r.clone_command.as_deref(), Some("mint clone {name}"));
    }

    #[test]
    fn parse_resolver_without_clone_command() {
        let toml = "[[repo-resolvers]]\n\
            name = \"corp-github\"\n\
            origin_pattern = \"git@github.example.com:corp/{name}.git\"\n";
        let cfg: CubeConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.repo_resolvers.len(), 1);
        assert_eq!(cfg.repo_resolvers[0].clone_command, None);
    }

    #[test]
    fn parse_multiple_resolvers_preserves_order() {
        let toml = "[[repo-resolvers]]\n\
            name = \"first\"\n\
            origin_pattern = \"git@a.example.com:x/{name}.git\"\n\
            [[repo-resolvers]]\n\
            name = \"second\"\n\
            origin_pattern = \"git@b.example.com:y/{name}.git\"\n";
        let cfg: CubeConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.repo_resolvers.len(), 2);
        assert_eq!(cfg.repo_resolvers[0].name, "first");
        assert_eq!(cfg.repo_resolvers[1].name, "second");
    }

    #[test]
    fn resolve_origin_substitutes_name() {
        let cfg: CubeConfig = toml::from_str(
            "[[repo-resolvers]]\n\
             name = \"mint\"\n\
             origin_pattern = \"org-1@github.com:linkedin-multiproduct/{name}.git\"\n\
             clone_command = \"mint clone {name}\"\n",
        )
        .expect("parse");
        let r = &cfg.repo_resolvers[0];
        assert_eq!(
            r.resolve_origin("frontend-api").as_deref(),
            Some("org-1@github.com:linkedin-multiproduct/frontend-api.git")
        );
        assert_eq!(
            r.resolve_clone_command("frontend-api").as_deref(),
            Some("mint clone frontend-api")
        );
    }

    #[test]
    fn unhealthy_gc_retention_defaults_to_twenty_four_hours() {
        // The old default was 5 days, which the observed pool never reached:
        // the oldest retained workspace was 4.2 days old while dispatch was
        // already failing. A threshold the steady state never crosses is not
        // a threshold.
        assert_eq!(UnhealthyGcConfig::default().max_age_secs(), 24 * 3_600);
    }

    #[test]
    fn unhealthy_gc_max_age_hours_wins_over_max_age_days() {
        let cfg: CubeConfig = toml::from_str("[unhealthy-gc]\nmax-age-hours = 6\nmax-age-days = 5\n").expect("parse");
        assert_eq!(cfg.unhealthy_gc.max_age_secs(), 6 * 3_600);
    }

    #[test]
    fn unhealthy_gc_max_age_days_still_honoured_for_existing_configs() {
        let cfg: CubeConfig = toml::from_str("[unhealthy-gc]\nmax-age-days = 2\n").expect("parse");
        assert_eq!(cfg.unhealthy_gc.max_age_secs(), 48 * 3_600);
    }

    #[test]
    fn pool_defaults_are_the_documented_ones() {
        let pool = PoolConfig::default();
        assert_eq!(pool.max_free_workspaces("mono"), DEFAULT_MAX_FREE_WORKSPACES);
        assert_eq!(pool.compact_idle_secs(), DEFAULT_COMPACT_IDLE_HOURS as i64 * 3_600);
        assert_eq!(pool.build_artifact_dirs(), vec!["target", ".build"]);
    }

    #[test]
    fn node_modules_is_not_reclaimed_by_default() {
        // It is an installed dependency tree coupled to cube's setup state,
        // not compiler output — opt-in only. See `default_build_artifact_dirs`.
        assert!(
            !PoolConfig::default()
                .build_artifact_dirs()
                .contains(&"node_modules".to_string())
        );
    }

    #[test]
    fn per_repo_mark_overrides_the_default() {
        let cfg: CubeConfig = toml::from_str(
            "[pool]\n\
             max-free-workspaces = 20\n\
             [pool.repos.flunge]\n\
             max-free-workspaces = 4\n",
        )
        .expect("parse");
        assert_eq!(cfg.pool.max_free_workspaces("flunge"), 4);
        // A repo with no entry falls back to the configured default.
        assert_eq!(cfg.pool.max_free_workspaces("mono"), 20);
    }

    #[test]
    fn free_space_floor_takes_the_larger_of_absolute_and_proportional() {
        let pool = PoolConfig::default();
        // 1.8 TiB: 2% (~36 GiB) dominates the 20 GiB absolute floor.
        let large = 1_800 * 1024 * 1024 * 1024_u64;
        assert_eq!(pool.free_space_floor_bytes(large), large / 100 * 2);
        // 200 GiB: 2% is 4 GiB, so the absolute floor dominates.
        let small = 200 * 1024 * 1024 * 1024_u64;
        assert_eq!(pool.free_space_floor_bytes(small), DEFAULT_MIN_FREE_BYTES);
    }

    #[test]
    fn free_space_floor_honours_explicit_config() {
        let cfg: CubeConfig = toml::from_str("[pool]\nmin-free-bytes = 1024\nmin-free-percent = 0\n").expect("parse");
        assert_eq!(cfg.pool.free_space_floor_bytes(1_000_000), 1024);
    }

    #[test]
    fn load_config_returns_default_when_file_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // CUBE_CONFIG_DIR points to a dir that exists but has no cube.toml
        // SAFETY: test-only; no other threads read this env var concurrently.
        unsafe { std::env::set_var("CUBE_CONFIG_DIR", tmp.path()) };
        let cfg = load_config().expect("load");
        unsafe { std::env::remove_var("CUBE_CONFIG_DIR") };
        assert!(cfg.repo_resolvers.is_empty());
    }
}
