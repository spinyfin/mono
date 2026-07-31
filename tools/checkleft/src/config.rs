use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::bypass::bypass_name_for_check_id;
use crate::external::ExternalCheckImplementationRef;
use crate::output::{Location, Severity};
use crate::path::validate_relative_path;
use crate::path_scope::PathScope;
use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::Deserialize;
use tracing::info;

const CHECKS_FILE_NAME_YAML: &str = "CHECKS.yaml";
const CHECKS_FILE_NAME_TOML: &str = "CHECKS.toml";
const CHECKS_CONFIG_DIAGNOSTIC_ID: &str = "checks-config";
const CHECKLEFT_HTTP_USER_AGENT: &str = "checkleft-cli";
const EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS: u32 = 5;
#[cfg(not(test))]
const EXTERNAL_CHECKS_FETCH_BASE_DELAY: Duration = Duration::from_millis(250);
#[cfg(test)]
const EXTERNAL_CHECKS_FETCH_BASE_DELAY: Duration = Duration::from_millis(5);
#[cfg(not(test))]
const EXTERNAL_CHECKS_FETCH_404_BASE_DELAY: Duration = Duration::from_secs(1);
#[cfg(test)]
const EXTERNAL_CHECKS_FETCH_404_BASE_DELAY: Duration = Duration::from_millis(20);
const EXTERNAL_CHECKS_MAX_CHAIN_DEPTH: usize = 8;
/// Canonical manifest filename for declarative-mode definitions within a
/// nested definition directory (legacy layout: `checks/<name>/check.yaml`).
const CHECK_DEF_FILE_NAME_YAML: &str = "check.yaml";
/// Canonical manifest filename for component-mode (`mode = "component"`)
/// definitions within a nested definition directory. `check.yaml` (declarative)
/// is checked first; `check.toml` (component) is tried when yaml is absent.
const CHECK_DEF_FILE_NAME_TOML: &str = "check.toml";

/// Resolved `check_definitions` section from a CHECKS file. Controls where
/// name-based definition resolution looks beyond the bundled set.
///
/// The default (zero config) is bundled-only resolution: a check whose `id`/`check`
/// names a bundled definition gets that def without any `implementation:` line.
/// `exec_paths` adds on-disk directories as an additional source; `allow_override_bundled`
/// controls precedence when both a bundled def and an exec-path def share a name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedCheckDefinitions {
    /// Repo-root-relative directories to search for check-definition files.
    /// Supports both flat layout (`<dir>/<name>.yaml`) and nested layout
    /// (`<dir>/<name>/check.yaml`). Flat is tried first. Empty by default.
    pub exec_paths: Vec<PathBuf>,
    /// When `true`, an exec-path def with the same name as a bundled def wins.
    /// Default `false` (bundled wins).
    pub allow_override_bundled: bool,
}

fn ensure_rustls_provider() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckConfig {
    pub check: String,
    pub id: String,
    pub source_path: PathBuf,
    /// Directory containing the CHECKS file that declared this check, relative to repo root.
    /// Empty path means the repo root itself.
    pub config_dir: PathBuf,
    pub origin: CheckConfigOrigin,
    pub implementation: Option<ExternalCheckImplementationRef>,
    pub enabled: bool,
    pub policy: CheckPolicyConfig,
    pub config: toml::Value,
    /// Per-check exclude patterns, already normalized to repo-root-relative coords.
    ///
    /// Sourced from the framework-level `exclude` / `exclude_files` / `exclude_globs` key
    /// on this check entry, plus the legacy `config.exclude_files` / `config.exclude_globs`
    /// position (backward-compat). Replaced (not unioned) on upsert, consistent with how
    /// the rest of a check entry is overridden by a child `CHECKS` file.
    pub exclude_patterns: Vec<String>,
    /// Per-check `applies_to` patterns (the framework include side), already
    /// normalized to repo-root-relative coords. Empty means the check entry
    /// declares no positive scope of its own — the check's definition scope
    /// (declarative manifest `applies_to`, a built-in's own predicate, or
    /// universal for a component) is what applies, unnarrowed.
    ///
    /// Sourced from the framework-level `applies_to` key on this check entry
    /// (sibling of `config:`), plus the legacy `config.applies_to` position
    /// (backward-compat, same shape as `exclude`'s legacy position). This
    /// entry-level list INTERSECTS the check's definition scope; it never
    /// replaces it. Replaced (not unioned) on upsert, consistent with
    /// `exclude_patterns`.
    pub applies_to_patterns: Vec<String>,
    /// What this check's scheduling is keyed on. See [`CheckScope`].
    pub scope: CheckScope,
}

/// What a configured check's scheduling is keyed on.
///
/// `Files` (the default) is today's behavior: the check is scheduled once per
/// distinct configuration and runs against whichever changed files matched it.
///
/// `Changeset` is for checks that have no file to point at — they inspect the
/// changeset as a whole (the PR description, the commit message) rather than
/// any one file. Such a check is scheduled exactly once per invocation,
/// regardless of the changed-file set (including an empty one), instead of
/// being tied to file resolution. See `Runner::schedule_changeset_scope_runs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckScope {
    #[default]
    Files,
    Changeset,
}

fn parse_check_scope(raw: &str) -> Result<CheckScope> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "files" => Ok(CheckScope::Files),
        "changeset" => Ok(CheckScope::Changeset),
        _ => bail!("expected one of `files` or `changeset`"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckConfigOrigin {
    Local,
    ExternalFile,
    ExternalUrl,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckPolicyConfig {
    pub severity: Option<Severity>,
    pub allow_bypass: Option<bool>,
    pub bypass_name: Option<String>,
    /// Per-check override of the stale-exclusion audit mode. `None` inherits the
    /// resolved global default (see [`ResolvedChecks::stale_exclusion_mode`]).
    pub stale_exclusion_mode: Option<StaleExclusionMode>,
    /// When `true`, restrict this check's findings to lines inside a PR-changed
    /// region of their file (in addition to the existing changed-*file*
    /// scoping). Opt-in; defaults to `false`, which preserves today's
    /// file-level-only scoping exactly.
    pub changed_lines_only: bool,
}

/// How the stale-exclusion audit reports a dead exclusion. Defaults to
/// [`Warn`](StaleExclusionMode::Warn); a repo can set it to
/// [`Error`](StaleExclusionMode::Error) to fail CI on dead exclusions, or
/// [`Off`](StaleExclusionMode::Off) to disable the audit entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StaleExclusionMode {
    Off,
    #[default]
    Warn,
    Error,
}

impl StaleExclusionMode {
    /// The finding severity for this mode, or `None` when the audit is disabled.
    pub fn severity(self) -> Option<Severity> {
        match self {
            Self::Off => None,
            Self::Warn => Some(Severity::Warning),
            Self::Error => Some(Severity::Error),
        }
    }
}

fn parse_stale_exclusion_mode(raw: &str) -> Result<StaleExclusionMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => Ok(StaleExclusionMode::Off),
        "warn" | "warning" => Ok(StaleExclusionMode::Warn),
        "error" => Ok(StaleExclusionMode::Error),
        _ => bail!("expected one of `off`, `warning`, or `error`"),
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedChecks {
    checks_by_id: BTreeMap<String, CheckConfig>,
    diagnostics: Vec<ConfigDiagnostic>,
    include_config_files: bool,
    stale_exclusion_mode: StaleExclusionMode,
    /// Accumulated check-definitions config (exec_paths + allow_override_bundled).
    /// Inherited down the config hierarchy and applied to each check's name-based resolution.
    check_definitions: ResolvedCheckDefinitions,
    /// Accumulated global exclude patterns (repo-root-relative) from all `CHECKS` files
    /// encountered from the repo root down to the resolved directory.
    ///
    /// These are unioned (accumulated) down the hierarchy: a child `CHECKS` file can only
    /// add more global excludes, never remove ones declared by an ancestor. Each pattern
    /// in this list is already normalized to repo-root-relative coords.
    global_exclude_patterns: Vec<String>,
}

impl ResolvedChecks {
    pub fn iter(&self) -> impl Iterator<Item = &CheckConfig> {
        self.checks_by_id.values()
    }

    pub fn enabled(&self) -> impl Iterator<Item = &CheckConfig> {
        self.checks_by_id.values().filter(|check| check.enabled)
    }

    pub fn get(&self, id: &str) -> Option<&CheckConfig> {
        self.checks_by_id.get(id)
    }

    pub fn diagnostics(&self) -> impl Iterator<Item = &ConfigDiagnostic> {
        self.diagnostics.iter()
    }

    pub fn include_config_files(&self) -> bool {
        self.include_config_files
    }

    /// The resolved global default mode for the stale-exclusion audit at this point in
    /// the config hierarchy. Per-check `policy.stale_exclusion_severity` overrides it.
    pub fn stale_exclusion_mode(&self) -> StaleExclusionMode {
        self.stale_exclusion_mode
    }

    /// The accumulated global exclude patterns for this directory, repo-root-relative.
    ///
    /// These are the union of every ancestor `CHECKS` file's top-level `exclude` (or
    /// `exclude_files` / `exclude_globs` alias) normalized to repo-root coords.
    pub fn global_exclude_patterns(&self) -> &[String] {
        &self.global_exclude_patterns
    }

    /// Strictly build the effective [`PathScope`] for config-time validation
    /// of a specific check instance. The positive side is `check`'s own
    /// `applies_to` patterns; the negative side is the union of:
    /// 1. the accumulated global exclude patterns for this directory
    /// 2. the per-check exclude patterns on `check`
    ///
    /// Returns an error if any pattern is invalid globset syntax. Run
    /// scheduling uses its own lenient construction so malformed patterns can
    /// be reported without preventing other checks from running.
    pub fn effective_matcher_for(&self, check: &CheckConfig) -> Result<PathScope> {
        let all: Vec<String> = self
            .global_exclude_patterns
            .iter()
            .chain(check.exclude_patterns.iter())
            .cloned()
            .collect();
        PathScope::new(&check.applies_to_patterns, &all)
    }

    fn upsert(&mut self, check: CheckConfig) {
        self.checks_by_id.insert(check.id.clone(), check);
    }

    fn push_diagnostic(&mut self, diagnostic: ConfigDiagnostic) {
        self.diagnostics.push(diagnostic);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub check_id: String,
    pub message: String,
    pub location: Location,
    pub remediation: Option<String>,
    /// Nearly all config diagnostics are hard errors (malformed CHECKS file, invalid
    /// field value) and skip loading the offending check. The exception is an unknown
    /// check-entry key during its deprecation window — see
    /// `unknown_check_field_severity` — which is diagnosed at `Warning` so the check
    /// still loads while the repo has time to fix it.
    pub severity: Severity,
}

#[derive(Debug)]
pub struct ConfigResolver {
    root: PathBuf,
    external_root_configs: Vec<LoadedChecksFile>,
    resolution_cache: Mutex<HashMap<PathBuf, ResolvedChecks>>,
    /// Severity for an unknown check-entry key, resolved once at construction from
    /// [`unknown_check_field_severity`]. Threading it through a field (rather than
    /// re-reading the build-time version on every resolution) lets tests pin an
    /// explicit severity via [`ConfigResolver::with_unknown_field_severity_for_test`]
    /// instead of depending on the ambient `CARGO_PKG_VERSION` the test binary was
    /// built with.
    unknown_field_severity: Severity,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigResolverOptions {
    pub external_checks_file: Option<String>,
    pub external_checks_url: Option<String>,
}

impl ConfigResolver {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = canonicalize_root(root.into())?;
        Ok(Self {
            root,
            external_root_configs: Vec::new(),
            resolution_cache: Mutex::new(HashMap::new()),
            unknown_field_severity: unknown_check_field_severity(),
        })
    }

    /// Test-only override so integration tests can pin the unknown-check-field-key
    /// severity explicitly instead of depending on the ambient `CARGO_PKG_VERSION`
    /// the test binary happened to be built with (which will start reporting
    /// `Error` once the deprecation window in [`UNKNOWN_CHECK_FIELD_WARN_THROUGH_VERSION`]
    /// closes).
    #[cfg(test)]
    fn with_unknown_field_severity_for_test(mut self, severity: Severity) -> Self {
        self.unknown_field_severity = severity;
        self
    }

    pub async fn new_with_options(root: impl Into<PathBuf>, options: ConfigResolverOptions) -> Result<Self> {
        let root = canonicalize_root(root.into())?;
        let external_root_configs =
            if let Some(external_checks_file) = normalize_optional_cli_value(options.external_checks_file) {
                if normalize_optional_cli_value(options.external_checks_url).is_some() {
                    bail!("only one of external checks file or external checks URL may be configured");
                }
                vec![load_external_checks_file_path(&root, &external_checks_file)?]
            } else if let Some(external_checks_url) = normalize_optional_cli_value(options.external_checks_url) {
                load_external_checks_chain(&external_checks_url).await?
            } else if let Some(external_checks_url) = discover_root_external_checks_url_for_prefetch(&root)? {
                load_external_checks_chain(&external_checks_url).await?
            } else {
                Vec::new()
            };

        Ok(Self {
            root,
            external_root_configs,
            resolution_cache: Mutex::new(HashMap::new()),
            unknown_field_severity: unknown_check_field_severity(),
        })
    }

    pub fn resolve_for_file(&self, file_path: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(file_path)?;
        info!(path = %file_path.display(), "resolving checks for file");

        self.resolve_for_dir(file_path.parent().unwrap_or(Path::new("")))
    }

    #[cfg(feature = "benchmarking")]
    #[doc(hidden)]
    pub fn resolve_for_file_without_cache(&self, file_path: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(file_path)?;

        self.resolve_for_dir_without_cache(file_path.parent().unwrap_or(Path::new("")))
    }

    fn resolve_for_dir(&self, relative_dir: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(relative_dir)?;

        if let Some(cached) = self.cached_resolution(relative_dir) {
            return Ok(cached);
        }

        let mut resolved = if relative_dir.as_os_str().is_empty() {
            self.base_resolution()?
        } else {
            self.resolve_for_dir(relative_dir.parent().unwrap_or(Path::new("")))?
        };

        self.apply_local_config(&mut resolved, relative_dir);
        self.store_cached_resolution(relative_dir, &resolved);
        Ok(resolved)
    }

    #[cfg(feature = "benchmarking")]
    fn resolve_for_dir_without_cache(&self, relative_dir: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(relative_dir)?;

        let mut resolved = if relative_dir.as_os_str().is_empty() {
            self.base_resolution()?
        } else {
            self.resolve_for_dir_without_cache(relative_dir.parent().unwrap_or(Path::new("")))?
        };

        self.apply_local_config(&mut resolved, relative_dir);
        Ok(resolved)
    }

    fn cached_resolution(&self, relative_dir: &Path) -> Option<ResolvedChecks> {
        self.resolution_cache
            .lock()
            .expect("config resolution cache poisoned")
            .get(relative_dir)
            .cloned()
    }

    fn store_cached_resolution(&self, relative_dir: &Path, resolved: &ResolvedChecks) {
        self.resolution_cache
            .lock()
            .expect("config resolution cache poisoned")
            .insert(relative_dir.to_path_buf(), resolved.clone());
    }

    fn base_resolution(&self) -> Result<ResolvedChecks> {
        let mut resolved = ResolvedChecks::default();
        for external_checks_file in &self.external_root_configs {
            apply_external_checks_file(&mut resolved, external_checks_file)?;
        }
        Ok(resolved)
    }

    fn apply_local_config(&self, resolved: &mut ResolvedChecks, relative_dir: &Path) {
        let config_abs_dir = self.root.join(relative_dir);
        let config_path = match resolve_checks_file_path(&config_abs_dir) {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(diagnostic) => {
                resolved.push_diagnostic(diagnostic);
                return;
            }
        };
        info!(path = %config_path.display(), "loading checks config");
        let config_relative_path = config_path
            .strip_prefix(&self.root)
            .unwrap_or(config_path.as_path())
            .to_path_buf();
        let check_config_dir = config_relative_path.parent().unwrap_or(Path::new("")).to_path_buf();

        let checks_file = match parse_checks_file(&config_path, &config_relative_path) {
            Ok(checks_file) => checks_file,
            Err(diagnostic) => {
                resolved.push_diagnostic(diagnostic);
                return;
            }
        };
        apply_local_settings(
            resolved,
            &checks_file.settings,
            config_abs_dir == self.root,
            &config_relative_path,
        );
        // Apply check_definitions if present (updates inherited state before the per-check loop).
        if let Some(raw_defs) = &checks_file.check_definitions {
            match parse_check_definitions(raw_defs, &config_relative_path) {
                Ok(defs) => resolved.check_definitions = defs,
                Err(diagnostic) => resolved.push_diagnostic(diagnostic),
            }
        }

        // Union this file's global excludes into the accumulated set. A structurally-empty
        // pattern is reported as a diagnostic but does not discard this file's other,
        // valid global excludes.
        let (global_excludes, global_exclude_diagnostics) =
            parse_global_exclude_patterns(&checks_file.exclude, &check_config_dir, &config_relative_path);
        resolved.global_exclude_patterns.extend(global_excludes);
        for diagnostic in global_exclude_diagnostics {
            resolved.push_diagnostic(diagnostic);
        }

        for check in checks_file.checks {
            let configured_id = check.id;
            if !check.unknown_fields.is_empty() {
                let severity = self.unknown_field_severity;
                for diagnostic in diagnose_unknown_check_fields(
                    &configured_id,
                    &check.unknown_fields,
                    &config_relative_path,
                    severity,
                ) {
                    resolved.push_diagnostic(diagnostic);
                }
                if severity == Severity::Error {
                    continue;
                }
            }
            // check_name is the definition name (check: field, defaulting to id).
            let check_name = check.check.as_deref().unwrap_or(&configured_id).to_owned();
            let implementation = if check.enabled {
                match resolve_check_implementation(
                    check.implementation.as_deref(),
                    &check_name,
                    &resolved.check_definitions,
                    Some(&self.root),
                    &configured_id,
                    None,
                ) {
                    Ok(implementation) => implementation,
                    Err(err) => {
                        resolved.push_diagnostic(config_check_diagnostic(
                            configured_id.clone(),
                            config_relative_path.clone(),
                            err.to_string(),
                        ));
                        continue;
                    }
                }
            } else {
                None
            };
            let policy = match parse_policy_config(&configured_id, &check.policy, check.enabled, None) {
                Ok(policy) => policy,
                Err(err) => {
                    resolved.push_diagnostic(config_check_diagnostic(
                        configured_id.clone(),
                        config_relative_path.clone(),
                        err.to_string(),
                    ));
                    continue;
                }
            };
            let exclude_patterns = match parse_per_check_exclude_patterns(
                &configured_id,
                &check.exclude,
                &check.config,
                &check_config_dir,
                &config_relative_path,
            ) {
                Ok(patterns) => patterns,
                Err(diagnostic) => {
                    resolved.push_diagnostic(diagnostic);
                    continue;
                }
            };
            let applies_to_patterns = match parse_per_check_applies_to_patterns(
                &configured_id,
                &check.applies_to,
                &check.config,
                &check_config_dir,
                &config_relative_path,
            ) {
                Ok((patterns, deprecation_warning)) => {
                    if let Some(diagnostic) = deprecation_warning {
                        resolved.push_diagnostic(diagnostic);
                    }
                    patterns
                }
                Err(diagnostic) => {
                    resolved.push_diagnostic(diagnostic);
                    continue;
                }
            };
            let scope_explicit = check.scope.is_some();
            let scope = match check.scope.as_deref() {
                Some(raw) => match parse_check_scope(raw) {
                    Ok(scope) => scope,
                    Err(err) => {
                        resolved.push_diagnostic(config_check_diagnostic(
                            configured_id.clone(),
                            config_relative_path.clone(),
                            format!("invalid `scope` for check `{configured_id}`: {err}"),
                        ));
                        continue;
                    }
                },
                // No `scope` key: an ancestor CHECKS file (typically the repo root, the
                // only place `scope = changeset` is honoured) may already have resolved
                // this id to `Changeset`. Inherit that rather than silently defaulting to
                // `Files` — otherwise a subdirectory override that only tweaks e.g.
                // `policy` flips the check to files scope for that subtree, and it ends
                // up scheduled twice (once as changeset at the root, once as files here).
                None => resolved
                    .get(&configured_id)
                    .map(|existing| existing.scope)
                    .unwrap_or_default(),
            };
            // Only an *explicit* `scope = changeset` in a subdirectory is rejected — it
            // can never be honoured there. An inherited `Changeset` scope (no `scope` key
            // in this file) is expected and must fall through to the upsert below so the
            // override still merges into the single root-scheduled changeset check.
            if scope_explicit && scope == CheckScope::Changeset && !check_config_dir.as_os_str().is_empty() {
                resolved.push_diagnostic(config_check_diagnostic(
                    configured_id.clone(),
                    config_relative_path.clone(),
                    format!(
                        "`scope = changeset` is only honoured in the repo-root CHECKS file; check `{configured_id}` declared in {} will never be scheduled",
                        config_relative_path.display()
                    ),
                ));
                continue;
            }
            // Validate a per-check `config.applies_to` override the same way the
            // top-level `exclude` list and per-check `exclude` list are validated: a
            // structurally-empty pattern here is otherwise only caught at check-execution
            // time by `override_applies_to` (declarative executor), which means a
            // config-only invocation (`list_configured_checks`) or a run that never
            // schedules this check (e.g. an empty changeset) never sees the problem.
            // Surface it here as well so it's visible at config-resolution time too;
            // `override_applies_to` remains as defence in depth for externally-supplied
            // config that bypasses this resolver.
            if let Some(toml::Value::Array(applies_to)) = check.config.get("applies_to") {
                for (i, entry) in applies_to.iter().enumerate() {
                    if let Some(s) = entry.as_str()
                        && let Some(reason) = crate::glob_scope::structurally_empty_reason(s, false)
                    {
                        resolved.push_diagnostic(config_check_diagnostic(
                            configured_id.clone(),
                            config_relative_path.clone(),
                            format!(
                                "`applies_to[{i}]` config override pattern `{s}` can never \
                                 match any changeset path: {reason}"
                            ),
                        ));
                    }
                }
            }
            // A `scope = changeset` check resolves once at repo root with an empty
            // changed-file set by construction (see `Runner::schedule_changeset_scope_runs`),
            // so a positive `applies_to` file-scope override on it is meaningless — it would
            // filter a file set that is always empty. Reject the combination rather than
            // silently accepting an override that can never do anything.
            if scope == CheckScope::Changeset && check.config.get("applies_to").is_some() {
                resolved.push_diagnostic(config_check_diagnostic(
                    configured_id.clone(),
                    config_relative_path.clone(),
                    format!(
                        "check `{configured_id}` sets `scope: changeset` and a `config.applies_to` \
                         override; a changeset-scope check runs once with no changed file to filter, \
                         so `applies_to` can never select anything here — remove it from this check's `config`"
                    ),
                ));
                continue;
            }
            resolved.upsert(CheckConfig {
                check: check_name,
                id: configured_id,
                source_path: config_relative_path.clone(),
                config_dir: check_config_dir.clone(),
                origin: CheckConfigOrigin::Local,
                implementation,
                enabled: check.enabled,
                policy,
                config: check.config,
                exclude_patterns,
                applies_to_patterns,
                scope,
            });
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ParsedChecksFile {
    #[serde(default)]
    settings: ParsedSettings,
    /// Top-level `check_definitions` section: controls where definition names
    /// are resolved (bundled vs. on-disk exec_paths).
    #[serde(default)]
    check_definitions: Option<ParsedCheckDefinitions>,
    #[serde(default)]
    checks: Vec<ParsedCheckConfig>,
    /// Top-level global excludes: paths matching these patterns are excluded from
    /// every check. `exclude` is the canonical name; `exclude_files` and `exclude_globs`
    /// are accepted aliases for backward compatibility.
    ///
    /// `None` means the key was absent (no global excludes declared here).
    /// `Some(vec![])` means the key was present but empty — rejected as an error.
    #[serde(default, alias = "exclude_files", alias = "exclude_globs")]
    exclude: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ParsedSettings {
    #[serde(default)]
    include_config_files: Option<bool>,
    #[serde(default)]
    external_checks_url: Option<String>,
    #[serde(default)]
    stale_exclusion_severity: Option<String>,
}

/// Parsed form of the top-level `check_definitions:` section.
#[derive(Debug, Clone, Default, Deserialize)]
struct ParsedCheckDefinitions {
    /// Relative directories containing check-definition yaml files.
    #[serde(default)]
    exec_paths: Vec<String>,
    /// When true, an exec-path def with the same name as a bundled def wins.
    #[serde(default)]
    allow_override_bundled: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ParsedCheckConfig {
    id: String,
    #[serde(default)]
    check: Option<String>,
    /// Explicit implementation reference (`generated:<id>`, `bundled:<name>`,
    /// or a repo-relative manifest path). When absent, the check name is resolved
    /// automatically against the bundled set and configured exec_paths.
    #[serde(default)]
    implementation: Option<String>,
    #[serde(default = "enabled_default")]
    enabled: bool,
    #[serde(default)]
    policy: ParsedCheckPolicyConfig,
    #[serde(default = "empty_toml_table")]
    config: toml::Value,
    /// Framework-level per-check excludes (sibling to `config:`, not inside it).
    ///
    /// `None` means absent; `Some(vec![])` is rejected as an error. The legacy
    /// in-`config` position (`config.exclude_files` / `config.exclude_globs`) is
    /// also read for backward compatibility and merged with this field.
    #[serde(default, alias = "exclude_files", alias = "exclude_globs")]
    exclude: Option<Vec<String>>,
    /// Framework-level per-check `applies_to` (sibling to `config:`, not inside
    /// it) — the include-side counterpart to `exclude`, occupying the position
    /// decision 3 of the unify-include-side design specifies.
    ///
    /// `None` means absent; `Some(vec![])` is rejected as an error. The legacy
    /// in-`config` position (`config.applies_to`) is also read for backward
    /// compatibility and merged with this field. This entry-level list
    /// INTERSECTS the check's definition scope (decision 2); it never replaces
    /// it — composition happens where the check's definition scope is known,
    /// not here.
    #[serde(default)]
    applies_to: Option<Vec<String>>,
    /// `files` (default) or `changeset`. See [`CheckScope`].
    #[serde(default)]
    scope: Option<String>,
    /// Any keys on this check entry that checkleft does not recognise, captured via
    /// `#[serde(flatten)]` rather than left to a bare `deny_unknown_fields` failure.
    /// Flattening lets us keep parsing the rest of this entry (and the rest of the
    /// file) and turn each stray key into a diagnostic that names the check id and
    /// suggests a correction, instead of aborting the whole CHECKS file on the first
    /// typo. See `diagnose_unknown_check_fields`.
    #[serde(flatten)]
    unknown_fields: BTreeMap<String, serde::de::IgnoredAny>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ParsedCheckPolicyConfig {
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    allow_bypass: Option<bool>,
    #[serde(default)]
    bypass_name: Option<String>,
    #[serde(default)]
    stale_exclusion_severity: Option<String>,
    /// `changed_lines_only`. See [`CheckPolicyConfig::changed_lines_only`].
    #[serde(default)]
    changed_lines_only: bool,
}

#[derive(Debug)]
struct LoadedChecksFile {
    origin: CheckConfigOrigin,
    source_label: String,
    source_path: PathBuf,
    parsed: ParsedChecksFile,
}

/// Normalize check-entry patterns (`exclude` or `applies_to`) from the `CHECKS`
/// file's directory to repo-root-relative.
///
/// Patterns are authored relative to the `CHECKS` file that declares them. This
/// function prefixes each pattern with `config_dir` so that matching can be done
/// against repo-root-relative changeset paths. A root config (`config_dir` empty)
/// requires no rewriting. `applies_to` uses this exact function so the include
/// and exclude sides share one coordinate system.
fn normalize_check_entry_patterns(patterns: &[String], config_dir: &Path) -> Vec<String> {
    if config_dir.as_os_str().is_empty() {
        return patterns.to_vec();
    }
    let prefix = config_dir.to_string_lossy();
    patterns.iter().map(|p| format!("{prefix}/{p}")).collect()
}

/// Extract the legacy per-check exclude patterns from the check's `config` blob.
///
/// Reads `exclude_files` and `exclude_globs` from the top-level keys of the
/// `config` TOML table, merging both, and normalizes them to repo-root-relative
/// using `config_dir`. Returns an empty `Vec` when neither key is present.
///
/// This preserves backward compatibility with the existing convention of placing
/// exclusion config inside the check's `config:` blob rather than at the
/// framework-level `exclude:` key. A structurally-empty pattern here is rejected
/// with a diagnostic exactly as it would be at the framework-level `exclude:` key
/// — this legacy position is a live, documented alias, not a lesser-validated one.
fn extract_legacy_config_excludes(
    check_id: &str,
    config: &toml::Value,
    config_dir: &Path,
    source_path: &Path,
) -> std::result::Result<Vec<String>, ConfigDiagnostic> {
    let Some(table) = config.as_table() else {
        return Ok(Vec::new());
    };
    let prefix = if config_dir.as_os_str().is_empty() {
        None
    } else {
        Some(config_dir.to_string_lossy().into_owned())
    };
    let mut patterns = Vec::new();
    for key in ["exclude_files", "exclude_globs"] {
        if let Some(toml::Value::Array(globs)) = table.get(key) {
            for (i, v) in globs.iter().enumerate() {
                if let Some(s) = v.as_str() {
                    if let Some(reason) = crate::glob_scope::structurally_empty_reason(s, true) {
                        return Err(config_file_diagnostic(
                            check_id.to_owned(),
                            source_path.to_path_buf(),
                            format!(
                                "check `{check_id}` `config.{key}[{i}]` pattern `{s}` can never \
                                 match any changeset path: {reason}"
                            ),
                            None,
                            None,
                            Some(format!(
                                "Fix or remove this pattern from this check's `config.{key}` list."
                            )),
                        ));
                    }
                    patterns.push(match &prefix {
                        Some(p) => format!("{p}/{s}"),
                        None => s.to_owned(),
                    });
                }
            }
        }
    }
    Ok(patterns)
}

/// Parse and validate global exclude patterns from a parsed `CHECKS` file, returning
/// them normalized to repo-root-relative coords alongside any diagnostics.
///
/// A structurally-empty pattern produces a diagnostic but does NOT discard the rest
/// of the list — the other, valid patterns are still returned and still apply. Only
/// an empty `exclude` key itself (no patterns at all) is a hard, no-patterns-returned
/// error, since there is nothing valid to salvage in that case.
fn parse_global_exclude_patterns(
    raw_exclude: &Option<Vec<String>>,
    config_dir: &Path,
    source_path: &Path,
) -> (Vec<String>, Vec<ConfigDiagnostic>) {
    let Some(patterns) = raw_exclude else {
        return (Vec::new(), Vec::new());
    };
    if patterns.is_empty() {
        return (
            Vec::new(),
            vec![config_file_diagnostic(
                CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
                source_path.to_path_buf(),
                "top-level `exclude` must not be an empty list; omit the key to declare no global excludes".to_owned(),
                None,
                None,
                Some("Add at least one glob pattern, or remove the `exclude` key entirely.".to_owned()),
            )],
        );
    }
    let mut valid = Vec::with_capacity(patterns.len());
    let mut diagnostics = Vec::new();
    for (i, pattern) in patterns.iter().enumerate() {
        if let Some(reason) = crate::glob_scope::structurally_empty_reason(pattern, true) {
            diagnostics.push(config_file_diagnostic(
                CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
                source_path.to_path_buf(),
                format!(
                    "top-level `exclude[{i}]` pattern `{pattern}` can never match any changeset \
                     path: {reason}"
                ),
                None,
                None,
                Some("Fix or remove this pattern from the top-level `exclude` list.".to_owned()),
            ));
        } else {
            valid.push(pattern.clone());
        }
    }
    let normalized = normalize_exclude_patterns(&valid, config_dir);
    if let Err(err) = ExclusionMatcher::new(&normalized) {
        diagnostics.push(config_file_diagnostic(
            CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            source_path.to_path_buf(),
            format!("invalid glob pattern in top-level `exclude`: {err}"),
            None,
            None,
            Some("Fix the invalid glob pattern in the `exclude` list.".to_owned()),
        ));
        return (Vec::new(), diagnostics);
    }
    (normalized, diagnostics)
}

/// Parse and validate per-check exclude patterns from a parsed check entry, returning
/// them normalized to repo-root-relative coords. Returns a diagnostic on error.
///
/// Merges the framework-level `exclude` field with the legacy `config.exclude_files` /
/// `config.exclude_globs` position for backward compatibility.
fn parse_per_check_exclude_patterns(
    check_id: &str,
    raw_exclude: &Option<Vec<String>>,
    config: &toml::Value,
    config_dir: &Path,
    source_path: &Path,
) -> std::result::Result<Vec<String>, ConfigDiagnostic> {
    if matches!(raw_exclude, Some(p) if p.is_empty()) {
        return Err(config_file_diagnostic(
            check_id.to_owned(),
            source_path.to_path_buf(),
            format!(
                "`exclude` for check `{check_id}` must not be an empty list; \
                 omit the key to declare no per-check excludes"
            ),
            None,
            None,
            Some("Add at least one glob pattern, or remove the `exclude` key from this check entry.".to_owned()),
        ));
    }
    if let Some(patterns) = raw_exclude {
        for (i, pattern) in patterns.iter().enumerate() {
            if let Some(reason) = crate::glob_scope::structurally_empty_reason(pattern, true) {
                return Err(config_file_diagnostic(
                    check_id.to_owned(),
                    source_path.to_path_buf(),
                    format!(
                        "check `{check_id}` `exclude[{i}]` pattern `{pattern}` can never match \
                         any changeset path: {reason}"
                    ),
                    None,
                    None,
                    Some("Fix or remove this pattern from this check's `exclude` list.".to_owned()),
                ));
            }
        }
    }
    let framework_patterns = raw_exclude
        .as_deref()
        .map(|p| normalize_check_entry_patterns(p, config_dir))
        .unwrap_or_default();
    let legacy_patterns = extract_legacy_config_excludes(check_id, config, config_dir, source_path)?;
    let all = [framework_patterns, legacy_patterns].concat();
    if let Err(err) = ExclusionMatcher::new(&all) {
        return Err(config_file_diagnostic(
            check_id.to_owned(),
            source_path.to_path_buf(),
            format!("invalid glob pattern in `exclude` for check `{check_id}`: {err}"),
            None,
            None,
            Some(format!(
                "Fix the invalid glob pattern in the `exclude` list for check `{check_id}`."
            )),
        ));
    }
    Ok(all)
}

/// Extract the legacy per-check `applies_to` override from the check's `config`
/// blob (`config.applies_to`), normalized to repo-root-relative using
/// `config_dir`. Returns `Ok(None)` when the key is absent — the legacy
/// position was not used at all, so no deprecation warning is due. Returns
/// `Ok(Some(patterns))` when the key is present and valid. Returns `Err` on a
/// malformed value: a scalar, an empty list, an empty-string entry, or a
/// non-string entry — mirroring the validation `resolve::override_applies_to`
/// performed before this rewiring.
///
/// This is the same legacy-position convention `extract_legacy_config_excludes`
/// implements for `exclude`: decision 3 of the unify-include-side design treats
/// the sibling-of-`config:` position and this in-`config` position as the same
/// one coordinate, so both normalize through [`normalize_check_entry_patterns`].
fn extract_legacy_config_applies_to(
    check_id: &str,
    config: &toml::Value,
    config_dir: &Path,
    source_path: &Path,
) -> std::result::Result<Option<Vec<String>>, ConfigDiagnostic> {
    let Some(table) = config.as_table() else {
        return Ok(None);
    };
    let Some(value) = table.get("applies_to") else {
        return Ok(None);
    };
    let malformed = |message: String| {
        config_file_diagnostic(
            check_id.to_owned(),
            source_path.to_path_buf(),
            message,
            None,
            None,
            Some("Fix `config.applies_to` for this check entry, or remove the key.".to_owned()),
        )
    };
    let array = match value.as_array() {
        Some(arr) => arr,
        None => {
            return Err(malformed(format!(
                "`config.applies_to` for check `{check_id}` must be a list of glob strings, not a scalar"
            )));
        }
    };
    if array.is_empty() {
        return Err(malformed(format!(
            "`config.applies_to` for check `{check_id}` must not be an empty list; \
             use `enabled: false` to disable the check instead"
        )));
    }
    let mut raw = Vec::with_capacity(array.len());
    for (i, entry) in array.iter().enumerate() {
        match entry.as_str() {
            Some(s) if !s.trim().is_empty() => raw.push(s.to_owned()),
            Some(_) => {
                return Err(malformed(format!(
                    "`config.applies_to[{i}]` for check `{check_id}` must not be an empty string"
                )));
            }
            None => {
                return Err(malformed(format!(
                    "`config.applies_to[{i}]` for check `{check_id}` must be a string"
                )));
            }
        }
    }
    Ok(Some(normalize_check_entry_patterns(&raw, config_dir)))
}

/// Parse and validate per-check `applies_to` patterns from a parsed check entry,
/// returning them normalized to repo-root-relative coords. Returns a diagnostic
/// on error.
///
/// Merges the framework-level `applies_to` field (sibling of `config:`) with the
/// legacy `config.applies_to` position for backward compatibility — mirroring
/// [`parse_per_check_exclude_patterns`] exactly, per decision 3 of the
/// unify-include-side design (one coordinate for both entry positions).
///
/// This is the check-entry INCLUDE side: it later INTERSECTS the check's
/// definition scope (decision 2) rather than replacing it — composition is
/// applied by the caller, not here. An empty (but present) list is rejected,
/// same as `exclude`'s precedent: "use `enabled: false` to disable the check
/// instead". A malformed legacy `config.applies_to` value (scalar, empty list,
/// or a non-string/empty-string entry) is rejected the same way, restoring the
/// validation `resolve::override_applies_to` performed before this rewiring.
///
/// On success, also returns an optional `Warning`-severity deprecation
/// [`ConfigDiagnostic`] when the legacy `config.applies_to` position was used
/// at all (present and valid) — the design's "accept both, emit a
/// `ConfigDiagnostic` at `warning`" requirement. The caller should push it
/// alongside the returned patterns without skipping the check.
fn parse_per_check_applies_to_patterns(
    check_id: &str,
    raw_applies_to: &Option<Vec<String>>,
    config: &toml::Value,
    config_dir: &Path,
    source_path: &Path,
) -> std::result::Result<(Vec<String>, Option<ConfigDiagnostic>), ConfigDiagnostic> {
    if matches!(raw_applies_to, Some(p) if p.is_empty()) {
        return Err(config_file_diagnostic(
            check_id.to_owned(),
            source_path.to_path_buf(),
            format!(
                "`applies_to` for check `{check_id}` must not be an empty list; \
                 use `enabled: false` to disable the check instead"
            ),
            None,
            None,
            Some("Add at least one glob pattern, or remove the `applies_to` key from this check entry.".to_owned()),
        ));
    }
    let framework_patterns = raw_applies_to
        .as_deref()
        .map(|p| normalize_check_entry_patterns(p, config_dir))
        .unwrap_or_default();
    let legacy_patterns = extract_legacy_config_applies_to(check_id, config, config_dir, source_path)?;
    let deprecation_warning = legacy_patterns.is_some().then(|| {
        config_file_diagnostic_with_severity(
            check_id.to_owned(),
            source_path.to_path_buf(),
            format!(
                "check `{check_id}` sets `applies_to` via the legacy `config.applies_to` position; \
                 prefer the top-level `applies_to` key on the check entry instead"
            ),
            None,
            None,
            Some(
                "Move the glob list from `config.applies_to` to a sibling `applies_to` key on this check entry."
                    .to_owned(),
            ),
            Severity::Warning,
        )
    });
    let patterns = [framework_patterns, legacy_patterns.unwrap_or_default()].concat();
    Ok((patterns, deprecation_warning))
}

fn parse_checks_file(path: &Path, relative_path: &Path) -> std::result::Result<ParsedChecksFile, ConfigDiagnostic> {
    let contents = fs::read_to_string(path).map_err(|err| {
        config_file_diagnostic(
            CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            relative_path.to_path_buf(),
            format!("failed to read checks config: {err}"),
            None,
            None,
            Some("Fix this CHECKS file so checkleft can load it.".to_owned()),
        )
    })?;
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");

    match extension {
        "yaml" | "yml" => serde_yaml::from_str(&contents).map_err(|err| yaml_parse_diagnostic(relative_path, err)),
        "toml" => toml::from_str(&contents).map_err(|err| toml_parse_diagnostic(relative_path, &contents, err)),
        _ => Err(config_file_diagnostic(
            CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            relative_path.to_path_buf(),
            "unsupported checks config extension (expected .yaml or .toml)".to_owned(),
            None,
            None,
            Some("Rename the file to CHECKS.yaml or CHECKS.toml.".to_owned()),
        )),
    }
}

fn parse_checks_contents(contents: &str, extension: &str, source_label: &str) -> Result<ParsedChecksFile> {
    match extension {
        "yaml" | "yml" => serde_yaml::from_str(contents).with_context(|| format!("failed to parse {source_label}")),
        "toml" => toml::from_str(contents).with_context(|| format!("failed to parse {source_label}")),
        _ => bail!(
            "unsupported checks config extension for {} (expected .yaml or .toml)",
            source_label
        ),
    }
}

fn enabled_default() -> bool {
    true
}

fn empty_toml_table() -> toml::Value {
    toml::Value::Table(Default::default())
}

fn parse_check_implementation(
    implementation: Option<&str>,
    check_id: &str,
    config_source: Option<&str>,
) -> Result<Option<ExternalCheckImplementationRef>> {
    let Some(implementation) = implementation else {
        return Ok(None);
    };
    let implementation = ExternalCheckImplementationRef::parse(implementation)
        .with_context(|| invalid_field_context("implementation", check_id, config_source))?;
    Ok(Some(implementation))
}

fn invalid_field_context(field: &str, check_id: &str, config_source: Option<&str>) -> String {
    match config_source {
        Some(config_source) => format!("invalid `{field}` for check `{check_id}` in {config_source}"),
        None => format!("invalid `{field}` for check `{check_id}`"),
    }
}

/// Validate and convert a [`ParsedCheckDefinitions`] to [`ResolvedCheckDefinitions`],
/// producing a config diagnostic on failure.
fn parse_check_definitions(
    raw: &ParsedCheckDefinitions,
    source_path: &Path,
) -> std::result::Result<ResolvedCheckDefinitions, ConfigDiagnostic> {
    let mut exec_paths = Vec::with_capacity(raw.exec_paths.len());
    for raw_path in &raw.exec_paths {
        let trimmed = raw_path.trim();
        if trimmed.is_empty() {
            return Err(config_file_diagnostic(
                CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
                source_path.to_path_buf(),
                "check_definitions.exec_paths entries must not be empty".to_owned(),
                None,
                None,
                Some("Provide a non-empty relative directory path.".to_owned()),
            ));
        }
        let path = PathBuf::from(trimmed);
        if let Err(err) = validate_relative_path(&path) {
            return Err(config_file_diagnostic(
                CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
                source_path.to_path_buf(),
                format!("invalid `check_definitions.exec_paths` entry `{trimmed}`: {err}"),
                None,
                None,
                Some("Each exec_paths entry must be a safe relative directory path.".to_owned()),
            ));
        }
        exec_paths.push(path);
    }
    Ok(ResolvedCheckDefinitions {
        exec_paths,
        allow_override_bundled: raw.allow_override_bundled,
    })
}

/// Resolve a check's external implementation reference.
///
/// - An explicit `implementation:` field (`generated:`, `bundled:`, or a path) is used verbatim.
/// - When `implementation:` is absent, `check_name` is resolved against the configured
///   definition sources in priority order:
///   1. `exec_paths` (if `allow_override_bundled: true`)
///   2. Bundled definitions embedded in the binary
///   3. `exec_paths` (if `allow_override_bundled: false`)
///   4. `None` — falls through to the Rust built-in registry at run time.
///
/// `repo_root` is `None` for remotely-fetched external configs, which may not reach into
/// the consuming repo's local filesystem; exec_paths are skipped in that case.
fn resolve_check_implementation(
    explicit_implementation: Option<&str>,
    check_name: &str,
    definitions: &ResolvedCheckDefinitions,
    repo_root: Option<&Path>,
    check_id: &str,
    config_source: Option<&str>,
) -> Result<Option<ExternalCheckImplementationRef>> {
    // Explicit `implementation:` field is used verbatim (for generated:, bundled:, or path).
    if let Some(raw_impl) = explicit_implementation {
        let raw_impl = raw_impl.trim();
        if raw_impl.is_empty() {
            bail!("`implementation` for check `{check_id}` must not be empty");
        }
        return parse_check_implementation(Some(raw_impl), check_id, config_source);
    }

    // No explicit implementation: resolve check_name against exec_paths and bundled defs.
    let in_bundled = is_bundled_check_name(check_name);

    // When allow_override_bundled, exec_paths take priority over bundled.
    if definitions.allow_override_bundled
        && let Some(root) = repo_root
        && let Some(file_ref) = find_in_exec_paths(&definitions.exec_paths, check_name, root)?
    {
        return Ok(Some(ExternalCheckImplementationRef::File(file_ref)));
    }

    // Bundled defs (embedded in binary, zero install).
    if in_bundled {
        return Ok(Some(ExternalCheckImplementationRef::Bundled(check_name.to_owned())));
    }

    // exec_paths at lower priority (when bundled does not win).
    if !definitions.allow_override_bundled
        && let Some(root) = repo_root
        && let Some(file_ref) = find_in_exec_paths(&definitions.exec_paths, check_name, root)?
    {
        return Ok(Some(ExternalCheckImplementationRef::File(file_ref)));
    }

    // Not found in bundled or exec_paths → None (falls through to Rust built-in registry).
    Ok(None)
}

/// Search `exec_paths` for a check manifest, returning the repo-root-relative
/// path if found. Two layouts are tried for each exec_path, flat first:
///
/// - Flat layout (preferred): `<exec_path>/<name>.yaml` / `.yml` / `.toml`
/// - Nested layout (legacy):  `<exec_path>/<name>/check.yaml` / `check.toml`
///
/// YAML is checked before TOML in both layouts. Returns `None` if no match
/// exists in any exec_path.
fn find_in_exec_paths(exec_paths: &[PathBuf], check_name: &str, repo_root: &Path) -> Result<Option<PathBuf>> {
    for exec_path in exec_paths {
        // Flat layout: <exec_path>/<check_name>.yaml / .yml / .toml
        for ext in ["yaml", "yml", "toml"] {
            let manifest_rel = exec_path.join(format!("{check_name}.{ext}"));
            let manifest_abs = repo_root.join(&manifest_rel);
            if manifest_abs.exists() {
                validate_relative_path(&manifest_rel).with_context(|| {
                    format!(
                        "resolved manifest path `{}` is not a safe relative path",
                        manifest_rel.display()
                    )
                })?;
                return Ok(Some(manifest_rel));
            }
        }
        // Nested layout (legacy): <exec_path>/<check_name>/check.yaml / check.toml
        for filename in [CHECK_DEF_FILE_NAME_YAML, CHECK_DEF_FILE_NAME_TOML] {
            let manifest_rel = exec_path.join(check_name).join(filename);
            let manifest_abs = repo_root.join(&manifest_rel);
            if manifest_abs.exists() {
                validate_relative_path(&manifest_rel).with_context(|| {
                    format!(
                        "resolved manifest path `{}` is not a safe relative path",
                        manifest_rel.display()
                    )
                })?;
                return Ok(Some(manifest_rel));
            }
        }
    }
    Ok(None)
}

/// Returns `true` when `name` matches a first-party definition embedded in the binary.
fn is_bundled_check_name(name: &str) -> bool {
    crate::external::bundled_check_names().any(|n| n == name)
}

fn parse_policy_config(
    check_id: &str,
    policy: &ParsedCheckPolicyConfig,
    enabled: bool,
    config_source: Option<&str>,
) -> Result<CheckPolicyConfig> {
    if !enabled {
        return Ok(CheckPolicyConfig::default());
    }

    let severity = match policy.severity.as_deref() {
        Some(raw) => Some(parse_policy_severity(raw).with_context(|| match config_source {
            Some(config_source) => {
                format!("invalid `policy.severity` for check `{check_id}` in {config_source}")
            }
            None => format!("invalid `policy.severity` for check `{check_id}`"),
        })?),
        None => None,
    };

    let bypass_name = policy
        .bypass_name
        .clone()
        .map(|raw| normalize_bypass_name(raw, check_id));

    let stale_exclusion_mode = match policy.stale_exclusion_severity.as_deref() {
        Some(raw) => Some(parse_stale_exclusion_mode(raw).with_context(|| match config_source {
            Some(config_source) => {
                format!("invalid `policy.stale_exclusion_severity` for check `{check_id}` in {config_source}")
            }
            None => format!("invalid `policy.stale_exclusion_severity` for check `{check_id}`"),
        })?),
        None => None,
    };

    Ok(CheckPolicyConfig {
        severity,
        allow_bypass: policy.allow_bypass,
        bypass_name,
        stale_exclusion_mode,
        changed_lines_only: policy.changed_lines_only,
    })
}

fn parse_policy_severity(raw: &str) -> Result<Severity> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => Ok(Severity::Error),
        "warning" => Ok(Severity::Warning),
        "info" => Ok(Severity::Info),
        _ => bail!("expected one of `error`, `warning`, or `info`"),
    }
}

fn normalize_bypass_name(raw: String, check_id: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return bypass_name_for_check_id(check_id);
    }
    if trimmed.to_ascii_uppercase().starts_with("BYPASS_") {
        return trimmed.to_ascii_uppercase();
    }

    bypass_name_for_check_id(trimmed)
}

fn yaml_parse_diagnostic(relative_path: &Path, err: serde_yaml::Error) -> ConfigDiagnostic {
    let location = err.location();
    config_file_diagnostic(
        CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
        relative_path.to_path_buf(),
        format!("failed to parse checks config: {err}"),
        location.as_ref().map(|location| location.line() as u32),
        location.as_ref().map(|location| location.column() as u32),
        Some("Fix YAML syntax so checkleft can load this CHECKS file.".to_owned()),
    )
}

fn toml_parse_diagnostic(relative_path: &Path, contents: &str, err: toml::de::Error) -> ConfigDiagnostic {
    let (line, column) = err
        .span()
        .map(|span| offset_to_line_column(contents, span.start))
        .map(|(line, column)| (Some(line), Some(column)))
        .unwrap_or((None, None));

    config_file_diagnostic(
        CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
        relative_path.to_path_buf(),
        format!("failed to parse checks config: {err}"),
        line,
        column,
        Some("Fix TOML syntax so checkleft can load this CHECKS file.".to_owned()),
    )
}

fn config_check_diagnostic(check_id: String, source_path: PathBuf, message: String) -> ConfigDiagnostic {
    config_file_diagnostic(
        check_id,
        source_path,
        message,
        None,
        None,
        Some("Fix this check entry in the CHECKS file.".to_owned()),
    )
}

fn config_file_diagnostic(
    check_id: String,
    path: PathBuf,
    message: String,
    line: Option<u32>,
    column: Option<u32>,
    remediation: Option<String>,
) -> ConfigDiagnostic {
    config_file_diagnostic_with_severity(check_id, path, message, line, column, remediation, Severity::Error)
}

fn config_file_diagnostic_with_severity(
    check_id: String,
    path: PathBuf,
    message: String,
    line: Option<u32>,
    column: Option<u32>,
    remediation: Option<String>,
    severity: Severity,
) -> ConfigDiagnostic {
    ConfigDiagnostic {
        check_id,
        message,
        location: Location { path, line, column },
        remediation,
        severity,
    }
}

/// Unknown check-entry keys remain warnings through the first release that carries
/// this behavior. The release pipeline increments the alpha component before it
/// builds, so the source tree's `0.1.0-alpha.8` becomes `0.1.0-alpha.9` there.
const UNKNOWN_CHECK_FIELD_WARN_THROUGH_VERSION: &str = "0.1.0-alpha.9";

fn unknown_check_field_severity() -> Severity {
    // CARGO_PKG_VERSION is always the package's real version: for a plain `cargo
    // build`/crates.io consumer it comes from Cargo.toml directly; for a Bazel
    // build it's injected from Cargo.toml via the `:cargo_toml_env_vars`
    // rustc_env_files target in BUILD.bazel, kept in sync automatically. The
    // release pipeline patches Cargo.toml's version before building, so this
    // reflects the version actually being released — no separate override needed.
    unknown_check_field_severity_for_version(env!("CARGO_PKG_VERSION"))
}

fn unknown_check_field_severity_for_version(effective_version: &str) -> Severity {
    if checkleft_alpha_version(effective_version)
        .zip(checkleft_alpha_version(UNKNOWN_CHECK_FIELD_WARN_THROUGH_VERSION))
        .is_some_and(|(effective, warn_through)| effective <= warn_through)
    {
        Severity::Warning
    } else {
        Severity::Error
    }
}

/// Parse the alpha-version shape accepted by the release pipeline. An unrecognised
/// version escalates immediately rather than silently extending the grace period.
fn checkleft_alpha_version(version: &str) -> Option<(u64, u64, u64, u64)> {
    let (base, alpha) = version.split_once("-alpha.")?;
    let mut components = base.split('.').map(str::parse::<u64>);
    let major = components.next()?.ok()?;
    let minor = components.next()?.ok()?;
    let patch = components.next()?.ok()?;
    if components.next().is_some() {
        return None;
    }
    Some((major, minor, patch, alpha.parse().ok()?))
}

/// Canonical field names on a check entry (`ParsedCheckConfig`), used to suggest a
/// correction for a misspelled key.
const KNOWN_CHECK_ENTRY_FIELDS: &[&str] = &[
    "id",
    "check",
    "implementation",
    "enabled",
    "policy",
    "config",
    "exclude",
    "applies_to",
    "scope",
];

/// Field names that only exist nested under `policy:`. One of these found as a
/// sibling of `policy:` — i.e. directly on the check entry — is a misplacement,
/// not a misspelling, and gets a more specific diagnostic than "unknown field".
const POLICY_ONLY_FIELD_NAMES: &[&str] = &[
    "severity",
    "allow_bypass",
    "bypass_name",
    "stale_exclusion_severity",
    "changed_lines_only",
];

/// Diagnose the unknown keys captured in `ParsedCheckConfig::unknown_fields`, naming
/// the offending key, the check id, and the CHECKS file, and suggesting a correction
/// when the key is a recognisable misspelling (edit distance <= 2 from a known field)
/// or misplacement (a `policy:`-only field written as a sibling of `policy:`).
fn diagnose_unknown_check_fields(
    check_id: &str,
    unknown_fields: &BTreeMap<String, serde::de::IgnoredAny>,
    source_path: &Path,
    severity: Severity,
) -> Vec<ConfigDiagnostic> {
    unknown_fields
        .keys()
        .map(|key| {
            let message = match suggest_check_field_correction(key) {
                Some(suggestion) => format!("unknown key `{key}` on check `{check_id}` — {suggestion}"),
                None => format!(
                    "unknown key `{key}` on check `{check_id}`; expected one of: {}",
                    KNOWN_CHECK_ENTRY_FIELDS.join(", ")
                ),
            };
            config_file_diagnostic_with_severity(
                check_id.to_owned(),
                source_path.to_path_buf(),
                message,
                None,
                None,
                Some("Fix or remove this key in the CHECKS file.".to_owned()),
                severity,
            )
        })
        .collect()
}

/// Suggest a correction for an unrecognised check-entry key, or `None` if it isn't a
/// recognisable misspelling or misplacement of a real field.
fn suggest_check_field_correction(key: &str) -> Option<String> {
    if POLICY_ONLY_FIELD_NAMES.contains(&key) {
        return Some(format!("`{key}` belongs inside `policy:`, not as a sibling of it"));
    }
    KNOWN_CHECK_ENTRY_FIELDS
        .iter()
        .map(|candidate| (*candidate, levenshtein_distance(key, candidate)))
        .filter(|(_, distance)| *distance <= 2)
        .min_by_key(|(_, distance)| *distance)
        .map(|(candidate, _)| format!("did you mean `{candidate}`?"))
}

/// Classic Levenshtein edit distance, used only to suggest corrections for a
/// misspelled check-entry key; not performance-sensitive (short strings, small sets).
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

fn offset_to_line_column(contents: &str, offset: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut column = 1u32;
    for (index, ch) in contents.char_indices() {
        if index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }

    (line, column)
}

fn canonicalize_root(root: PathBuf) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize root {}", root.display()))?;
    if !root.is_dir() {
        bail!("config resolver root is not a directory: {}", root.display());
    }

    Ok(root)
}

fn apply_local_settings(
    resolved: &mut ResolvedChecks,
    settings: &ParsedSettings,
    is_root_config: bool,
    source_path: &Path,
) {
    if let Some(include_config_files) = settings.include_config_files {
        resolved.include_config_files = include_config_files;
    }

    if let Some(raw) = settings.stale_exclusion_severity.as_deref() {
        match parse_stale_exclusion_mode(raw) {
            Ok(mode) => resolved.stale_exclusion_mode = mode,
            Err(error) => resolved.push_diagnostic(config_file_diagnostic(
                CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
                source_path.to_path_buf(),
                format!("invalid `settings.stale_exclusion_severity`: {error}"),
                None,
                None,
                Some("Set `settings.stale_exclusion_severity` to `off`, `warning`, or `error`.".to_owned()),
            )),
        }
    }

    let Some(external_checks_url) = settings.external_checks_url.as_deref() else {
        return;
    };
    if !is_root_config {
        resolved.push_diagnostic(config_file_diagnostic(
            CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            source_path.to_path_buf(),
            "`settings.external_checks_url` is only supported in the repository root config".to_owned(),
            None,
            None,
            Some("Remove `settings.external_checks_url` from child CHECKS files.".to_owned()),
        ));
        return;
    }

    if let Err(error) = validate_external_checks_url(external_checks_url, None) {
        resolved.push_diagnostic(config_file_diagnostic(
            CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            source_path.to_path_buf(),
            format!("invalid `settings.external_checks_url`: {error}"),
            None,
            None,
            Some("Set `settings.external_checks_url` to a valid absolute URL.".to_owned()),
        ));
    }
}

fn apply_external_checks_file(resolved: &mut ResolvedChecks, external_checks_file: &LoadedChecksFile) -> Result<()> {
    if let Some(include_config_files) = external_checks_file.parsed.settings.include_config_files {
        resolved.include_config_files = include_config_files;
    }

    if let Some(raw) = external_checks_file.parsed.settings.stale_exclusion_severity.as_deref() {
        resolved.stale_exclusion_mode = parse_stale_exclusion_mode(raw).with_context(|| {
            format!(
                "invalid `settings.stale_exclusion_severity` in {}",
                external_checks_file.source_label
            )
        })?;
    }

    // Reject exec_paths in external configs — they would reach into the consuming
    // repo's local filesystem, which is the same trust boundary as a File implementation ref.
    if let Some(raw_defs) = &external_checks_file.parsed.check_definitions
        && !raw_defs.exec_paths.is_empty()
    {
        bail!(
            "`check_definitions.exec_paths` is not allowed in an external checks config ({}); \
             a directory path source would reach into the consuming repo's local filesystem",
            external_checks_file.source_label
        );
    }
    // allow_override_bundled without exec_paths is a no-op for remote configs, ignore.

    // External configs always have config_dir = "" (root). Union their global excludes.
    if let Some(patterns) = &external_checks_file.parsed.exclude {
        if patterns.is_empty() {
            bail!(
                "top-level `exclude` in {} must not be an empty list; \
                 omit the key to declare no global excludes",
                external_checks_file.source_label
            );
        }
        if let Err(err) = ExclusionMatcher::new(patterns) {
            bail!(
                "invalid glob pattern in top-level `exclude` in {}: {err}",
                external_checks_file.source_label
            );
        }
        resolved.global_exclude_patterns.extend(patterns.iter().cloned());
    }

    for check in &external_checks_file.parsed.checks {
        let configured_id = check.id.clone();
        let check_name = check.check.clone().unwrap_or_else(|| configured_id.clone());
        let implementation = if check.enabled {
            // External configs may not use exec_paths (no repo_root passed).
            // Bundled resolution still applies (zero-install path).
            let implementation = resolve_check_implementation(
                check.implementation.as_deref(),
                &check_name,
                &resolved.check_definitions,
                None,
                &configured_id,
                Some(&external_checks_file.source_label),
            )?;
            validate_external_root_check_implementation(
                external_checks_file.origin,
                implementation.as_ref(),
                &configured_id,
                &external_checks_file.source_label,
            )?;
            implementation
        } else {
            None
        };
        let policy = parse_policy_config(
            &configured_id,
            &check.policy,
            check.enabled,
            Some(&external_checks_file.source_label),
        )?;
        // For external configs, config_dir is always empty (root-level).
        let exclude_patterns = if let Some(patterns) = &check.exclude {
            if patterns.is_empty() {
                bail!(
                    "`exclude` for check `{configured_id}` in {} must not be an empty list; \
                         omit the key to declare no per-check excludes",
                    external_checks_file.source_label
                );
            }
            let mut all = patterns.clone();
            all.extend(
                extract_legacy_config_excludes(&configured_id, &check.config, Path::new(""), Path::new(""))
                    .map_err(|diag| anyhow::anyhow!(diag.message))?,
            );
            if let Err(err) = ExclusionMatcher::new(&all) {
                bail!(
                    "invalid glob pattern in `exclude` for check `{configured_id}` in {}: {err}",
                    external_checks_file.source_label
                );
            }
            all
        } else {
            let patterns = extract_legacy_config_excludes(&configured_id, &check.config, Path::new(""), Path::new(""))
                .map_err(|diag| anyhow::anyhow!(diag.message))?;
            if let Err(err) = ExclusionMatcher::new(&patterns) {
                bail!(
                    "invalid glob pattern in legacy `config.exclude_files`/`config.exclude_globs` \
                     for check `{configured_id}` in {}: {err}",
                    external_checks_file.source_label
                );
            }
            patterns
        };
        let legacy_applies_to =
            extract_legacy_config_applies_to(&configured_id, &check.config, Path::new(""), Path::new(""))
                .map_err(|diagnostic| anyhow::anyhow!(diagnostic.message))?
                .unwrap_or_default();
        let applies_to_patterns = if let Some(patterns) = &check.applies_to {
            if patterns.is_empty() {
                bail!(
                    "`applies_to` for check `{configured_id}` in {} must not be an empty list; \
                         use `enabled: false` to disable the check instead",
                    external_checks_file.source_label
                );
            }
            let mut all = patterns.clone();
            all.extend(legacy_applies_to);
            all
        } else {
            legacy_applies_to
        };
        let scope = match check.scope.as_deref() {
            Some(raw) => parse_check_scope(raw).with_context(|| {
                format!(
                    "invalid `scope` for check `{configured_id}` in {}",
                    external_checks_file.source_label
                )
            })?,
            None => CheckScope::default(),
        };
        resolved.upsert(CheckConfig {
            check: check_name,
            id: configured_id,
            source_path: external_checks_file.source_path.clone(),
            config_dir: PathBuf::new(),
            origin: external_checks_file.origin,
            implementation,
            enabled: check.enabled,
            policy,
            config: check.config.clone(),
            exclude_patterns,
            applies_to_patterns,
            scope,
        });
    }

    Ok(())
}

fn discover_root_external_checks_url_for_prefetch(root: &Path) -> Result<Option<String>> {
    let Ok(Some(config_path)) = resolve_checks_file_path(root) else {
        return Ok(None);
    };
    let relative_path = config_path
        .strip_prefix(root)
        .unwrap_or(config_path.as_path())
        .to_path_buf();
    let Ok(checks_file) = parse_checks_file(&config_path, &relative_path) else {
        return Ok(None);
    };
    let Some(external_checks_url) = checks_file.settings.external_checks_url else {
        return Ok(None);
    };
    let Some(external_checks_url) = normalize_optional_cli_value(Some(external_checks_url)) else {
        return Ok(None);
    };
    if resolve_external_checks_url(&external_checks_url, None).is_err() {
        return Ok(None);
    }
    Ok(Some(external_checks_url))
}

fn load_external_checks_file_path(root: &Path, external_checks_file: &str) -> Result<LoadedChecksFile> {
    let resolved_path = resolve_external_checks_file_path(root, external_checks_file)?;
    let source_label = resolved_path.display().to_string();
    let parsed = parse_checks_contents_from_path(&resolved_path, &source_label)?;

    Ok(LoadedChecksFile {
        origin: CheckConfigOrigin::ExternalFile,
        source_label,
        source_path: resolved_path,
        parsed,
    })
}

fn normalize_optional_cli_value(raw: Option<String>) -> Option<String> {
    raw.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn validate_external_checks_url(raw_url: &str, base_url: Option<&reqwest::Url>) -> Result<()> {
    resolve_external_checks_url(raw_url, base_url).map(|_| ())
}

fn resolve_external_checks_file_path(root: &Path, raw_path: &str) -> Result<PathBuf> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        bail!("external checks file path must not be empty");
    }

    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        return Ok(path);
    }

    validate_relative_path(&path).context("external checks file path must be a safe relative path")?;
    Ok(root.join(path))
}

fn parse_checks_contents_from_path(path: &Path, source_label: &str) -> Result<ParsedChecksFile> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read external checks config {source_label}"))?;
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    parse_checks_contents(&contents, extension, source_label)
        .with_context(|| format!("failed to parse external checks config {source_label}"))
}

fn normalize_configured_external_checks_url(raw: Option<String>, source_label: &str) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("`settings.external_checks_url` in {source_label} must not be empty");
    }
    Ok(Some(trimmed.to_owned()))
}

async fn load_external_checks_chain(external_checks_url: &str) -> Result<Vec<LoadedChecksFile>> {
    info!(external_checks_url, "loading external checks config chain");
    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .user_agent(CHECKLEFT_HTTP_USER_AGENT)
        .build()
        .context("failed to build HTTP client for external checks config")?;
    let mut seen_urls = HashSet::new();
    let mut loaded = Vec::new();
    let mut next_url = Some(resolve_external_checks_url(external_checks_url, None)?);

    while let Some(url) = next_url {
        if loaded.len() >= EXTERNAL_CHECKS_MAX_CHAIN_DEPTH {
            bail!(
                "external checks config chain exceeded {} entries while loading {}",
                EXTERNAL_CHECKS_MAX_CHAIN_DEPTH,
                external_checks_url
            );
        }
        if !seen_urls.insert(url.as_str().to_owned()) {
            bail!("external checks config cycle detected at {}", url);
        }

        let fetched = fetch_external_checks_file(&client, url.clone()).await?;
        next_url = normalize_configured_external_checks_url(
            fetched.parsed.settings.external_checks_url.clone(),
            &fetched.source_label,
        )?
        .map(|nested_url| resolve_external_checks_url(&nested_url, Some(&url)))
        .transpose()?;
        loaded.push(fetched);
    }

    loaded.reverse();
    Ok(loaded)
}

fn resolve_external_checks_url(raw_url: &str, base_url: Option<&reqwest::Url>) -> Result<reqwest::Url> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        bail!("external checks URL must not be empty");
    }

    let parsed = match base_url {
        Some(base_url) => base_url.join(trimmed).or_else(|_| reqwest::Url::parse(trimmed)),
        None => reqwest::Url::parse(trimmed),
    };

    parsed.with_context(|| format!("invalid external checks URL `{trimmed}`"))
}

fn validate_external_root_check_implementation(
    origin: CheckConfigOrigin,
    implementation: Option<&ExternalCheckImplementationRef>,
    check_id: &str,
    source_label: &str,
) -> Result<()> {
    let Some(implementation) = implementation else {
        return Ok(());
    };

    if origin == CheckConfigOrigin::ExternalFile && matches!(implementation, ExternalCheckImplementationRef::File(_)) {
        bail!(
            "invalid `implementation` for check `{check_id}` in {source_label}: external checks files may only use `generated:` or `bundled:` implementations"
        );
    }

    Ok(())
}

async fn fetch_external_checks_file(client: &reqwest::Client, url: reqwest::Url) -> Result<LoadedChecksFile> {
    let mut last_retryable_error = None;

    for attempt in 1..=EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
        info!(
            url = %url,
            attempt,
            max_attempts = EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS,
            "fetching external checks config"
        );
        match client.get(url.clone()).send().await {
            Ok(response) => {
                let status = response.status();
                if status == StatusCode::NOT_FOUND {
                    if attempt == EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
                        bail!(
                            "external checks config {} returned 404 Not Found after {} attempts",
                            url,
                            EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS
                        );
                    }
                    tokio::time::sleep(external_checks_retry_delay(attempt, status)).await;
                    continue;
                }

                if !status.is_success() {
                    let response_error = format!("external checks config {} returned {}", url, status);
                    if is_retryable_http_status(status) {
                        last_retryable_error = Some(response_error);
                        if attempt == EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
                            break;
                        }
                        tokio::time::sleep(external_checks_retry_delay(attempt, status)).await;
                        continue;
                    }
                    bail!("{response_error}");
                }

                let contents = match response.text().await {
                    Ok(contents) => contents,
                    Err(error) => {
                        last_retryable_error = Some(format!(
                            "failed to read external checks config {} response body: {error}",
                            url
                        ));
                        if attempt == EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
                            break;
                        }
                        tokio::time::sleep(external_checks_retry_delay(attempt, status)).await;
                        continue;
                    }
                };
                let extension = Path::new(url.path())
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("");
                let parsed = parse_checks_contents(&contents, extension, url.as_str())
                    .with_context(|| format!("failed to parse external checks config {url}"))?;
                return Ok(LoadedChecksFile {
                    origin: CheckConfigOrigin::ExternalUrl,
                    source_label: url.to_string(),
                    source_path: PathBuf::from(url.as_str()),
                    parsed,
                });
            }
            Err(error) => {
                last_retryable_error = Some(format!("failed to retrieve external checks config {}: {error}", url));
                if attempt == EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS {
                    break;
                }
                tokio::time::sleep(external_checks_retry_delay(attempt, StatusCode::REQUEST_TIMEOUT)).await;
            }
        }
    }

    let message = last_retryable_error.unwrap_or_else(|| format!("failed to retrieve external checks config {}", url));
    bail!("{message} after {} attempts", EXTERNAL_CHECKS_FETCH_MAX_ATTEMPTS)
}

fn is_retryable_http_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn external_checks_retry_delay(attempt: u32, status: StatusCode) -> Duration {
    let base_delay = if status == StatusCode::NOT_FOUND {
        EXTERNAL_CHECKS_FETCH_404_BASE_DELAY
    } else {
        EXTERNAL_CHECKS_FETCH_BASE_DELAY
    };
    base_delay.saturating_mul(2_u32.saturating_pow(attempt.saturating_sub(1)))
}

fn resolve_checks_file_path(dir: &Path) -> Result<Option<PathBuf>, ConfigDiagnostic> {
    let yaml_path = dir.join(CHECKS_FILE_NAME_YAML);
    let toml_path = dir.join(CHECKS_FILE_NAME_TOML);
    let yaml_exists = yaml_path.exists();
    let toml_exists = toml_path.exists();

    if yaml_exists && toml_exists {
        return Err(ConfigDiagnostic {
            check_id: CHECKS_CONFIG_DIAGNOSTIC_ID.to_owned(),
            message: format!(
                "both {} and {} exist in the same directory — keep exactly one",
                yaml_path.display(),
                toml_path.display(),
            ),
            location: Location {
                path: yaml_path,
                line: None,
                column: None,
            },
            remediation: Some("Remove one of the two config files so checkleft knows which one to load.".to_owned()),
            severity: Severity::Error,
        });
    }

    if yaml_exists {
        return Ok(Some(yaml_path));
    }
    if toml_exists {
        return Ok(Some(toml_path));
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
