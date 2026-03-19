use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::bypass::bypass_name_for_check_id;
use crate::external::ExternalCheckImplementationRef;
use crate::output::Severity;
use crate::path::validate_relative_path;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

const CHECKS_FILE_NAME_YAML: &str = "CHECKS.yaml";
const CHECKS_FILE_NAME_TOML: &str = "CHECKS.toml";

#[derive(Debug, Clone, PartialEq)]
pub struct CheckConfig {
    pub check: String,
    pub id: String,
    pub implementation: Option<ExternalCheckImplementationRef>,
    pub enabled: bool,
    pub policy: CheckPolicyConfig,
    pub config: toml::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckPolicyConfig {
    pub severity: Option<Severity>,
    pub allow_bypass: Option<bool>,
    pub bypass_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedChecks {
    checks_by_id: BTreeMap<String, CheckConfig>,
    include_config_files: bool,
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

    pub fn include_config_files(&self) -> bool {
        self.include_config_files
    }

    fn upsert(&mut self, check: CheckConfig) {
        self.checks_by_id.insert(check.id.clone(), check);
    }
}

#[derive(Debug)]
pub struct ConfigResolver {
    root: PathBuf,
}

impl ConfigResolver {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize root {}", root.display()))?;
        if !root.is_dir() {
            bail!(
                "config resolver root is not a directory: {}",
                root.display()
            );
        }

        Ok(Self { root })
    }

    pub fn resolve_for_file(&self, file_path: &Path) -> Result<ResolvedChecks> {
        validate_relative_path(file_path)?;

        let parent_dir = file_path.parent().unwrap_or(Path::new(""));
        let search_dirs = root_to_leaf_dirs(parent_dir)?;

        let mut resolved = ResolvedChecks::default();
        for relative_dir in search_dirs {
            let config_dir = self.root.join(relative_dir);
            let Some(config_path) = resolve_checks_file_path(&config_dir) else {
                continue;
            };

            let checks_file = parse_checks_file(&config_path)?;
            if let Some(include_config_files) = checks_file.settings.include_config_files {
                resolved.include_config_files = include_config_files;
            }
            for check in checks_file.checks {
                let configured_id = check.id;
                let implementation = if check.enabled {
                    parse_check_implementation(check.implementation, &config_path, &configured_id)?
                } else {
                    None
                };
                let policy =
                    parse_policy_config(&configured_id, check.policy, check.enabled, &config_path)?;
                resolved.upsert(CheckConfig {
                    check: check.check.unwrap_or_else(|| configured_id.clone()),
                    id: configured_id,
                    implementation,
                    enabled: check.enabled,
                    policy,
                    config: check.config,
                });
            }
        }

        Ok(resolved)
    }
}

#[derive(Debug, Deserialize)]
struct ParsedChecksFile {
    #[serde(default)]
    settings: ParsedSettings,
    #[serde(default)]
    checks: Vec<ParsedCheckConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct ParsedSettings {
    #[serde(default)]
    include_config_files: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ParsedCheckConfig {
    id: String,
    #[serde(default)]
    check: Option<String>,
    #[serde(default)]
    implementation: Option<String>,
    #[serde(default = "enabled_default")]
    enabled: bool,
    #[serde(default)]
    policy: ParsedCheckPolicyConfig,
    #[serde(default = "empty_toml_table")]
    config: toml::Value,
}

#[derive(Debug, Default, Deserialize)]
struct ParsedCheckPolicyConfig {
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    allow_bypass: Option<bool>,
    #[serde(default)]
    bypass_name: Option<String>,
}

fn parse_checks_file(path: &Path) -> Result<ParsedChecksFile> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");

    match extension {
        "yaml" | "yml" => serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display())),
        "toml" => {
            toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
        }
        _ => bail!(
            "unsupported checks config extension for {} (expected .yaml or .toml)",
            path.display()
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
    implementation: Option<String>,
    config_path: &Path,
    check_id: &str,
) -> Result<Option<ExternalCheckImplementationRef>> {
    let Some(implementation) = implementation else {
        return Ok(None);
    };
    let implementation =
        ExternalCheckImplementationRef::parse(&implementation).with_context(|| {
            format!(
                "invalid `implementation` for check `{check_id}` in {}",
                config_path.display()
            )
        })?;
    Ok(Some(implementation))
}

fn parse_policy_config(
    check_id: &str,
    policy: ParsedCheckPolicyConfig,
    enabled: bool,
    config_path: &Path,
) -> Result<CheckPolicyConfig> {
    if !enabled {
        return Ok(CheckPolicyConfig::default());
    }

    let severity = match policy.severity {
        Some(raw) => Some(parse_policy_severity(&raw).with_context(|| {
            format!(
                "invalid `policy.severity` for check `{check_id}` in {}",
                config_path.display()
            )
        })?),
        None => None,
    };

    let bypass_name = policy
        .bypass_name
        .map(|raw| normalize_bypass_name(raw, check_id));

    Ok(CheckPolicyConfig {
        severity,
        allow_bypass: policy.allow_bypass,
        bypass_name,
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

fn root_to_leaf_dirs(path: &Path) -> Result<Vec<PathBuf>> {
    validate_relative_path(path)?;

    let mut output = vec![PathBuf::new()];
    let mut current = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(part) = component {
            current.push(part);
            output.push(current.clone());
        }
    }

    Ok(output)
}

fn resolve_checks_file_path(dir: &Path) -> Option<PathBuf> {
    let yaml_path = dir.join(CHECKS_FILE_NAME_YAML);
    if yaml_path.exists() {
        return Some(yaml_path);
    }

    let toml_path = dir.join(CHECKS_FILE_NAME_TOML);
    if toml_path.exists() {
        return Some(toml_path);
    }

    None
}

#[cfg(test)]
mod tests;
