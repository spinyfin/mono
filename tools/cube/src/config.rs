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
