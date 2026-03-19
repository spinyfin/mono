use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::bypass::bypass_name_for_check_id;
use crate::external::ExternalCheckImplementationRef;
use crate::output::{Location, Severity};
use crate::path::validate_relative_path;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

const CHECKS_FILE_NAME_YAML: &str = "CHECKS.yaml";
const CHECKS_FILE_NAME_TOML: &str = "CHECKS.toml";
const CHECKS_CONFIG_DIAGNOSTIC_ID: &str = "checks-config";

#[derive(Debug, Clone, PartialEq)]
pub struct CheckConfig {
    pub check: String,
    pub id: String,
    pub source_path: PathBuf,
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
    diagnostics: Vec<ConfigDiagnostic>,
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

    pub fn diagnostics(&self) -> impl Iterator<Item = &ConfigDiagnostic> {
        self.diagnostics.iter()
    }

    pub fn include_config_files(&self) -> bool {
        self.include_config_files
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
            let config_relative_path = config_path
                .strip_prefix(&self.root)
                .unwrap_or(config_path.as_path())
                .to_path_buf();

            let checks_file = match parse_checks_file(&config_path, &config_relative_path) {
                Ok(checks_file) => checks_file,
                Err(diagnostic) => {
                    resolved.push_diagnostic(diagnostic);
                    continue;
                }
            };
            if let Some(include_config_files) = checks_file.settings.include_config_files {
                resolved.include_config_files = include_config_files;
            }
            for check in checks_file.checks {
                let configured_id = check.id;
                let implementation = if check.enabled {
                    match parse_check_implementation(check.implementation, &configured_id) {
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
                let policy = match parse_policy_config(&configured_id, check.policy, check.enabled)
                {
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
                resolved.upsert(CheckConfig {
                    check: check.check.unwrap_or_else(|| configured_id.clone()),
                    id: configured_id,
                    source_path: config_relative_path.clone(),
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

fn parse_checks_file(
    path: &Path,
    relative_path: &Path,
) -> std::result::Result<ParsedChecksFile, ConfigDiagnostic> {
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
        "yaml" | "yml" => {
            serde_yaml::from_str(&contents).map_err(|err| yaml_parse_diagnostic(relative_path, err))
        }
        "toml" => toml::from_str(&contents)
            .map_err(|err| toml_parse_diagnostic(relative_path, &contents, err)),
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

fn enabled_default() -> bool {
    true
}

fn empty_toml_table() -> toml::Value {
    toml::Value::Table(Default::default())
}

fn parse_check_implementation(
    implementation: Option<String>,
    check_id: &str,
) -> Result<Option<ExternalCheckImplementationRef>> {
    let Some(implementation) = implementation else {
        return Ok(None);
    };
    let implementation = ExternalCheckImplementationRef::parse(&implementation)
        .with_context(|| format!("invalid `implementation` for check `{check_id}`"))?;
    Ok(Some(implementation))
}

fn parse_policy_config(
    check_id: &str,
    policy: ParsedCheckPolicyConfig,
    enabled: bool,
) -> Result<CheckPolicyConfig> {
    if !enabled {
        return Ok(CheckPolicyConfig::default());
    }

    let severity = match policy.severity {
        Some(raw) => Some(
            parse_policy_severity(&raw)
                .with_context(|| format!("invalid `policy.severity` for check `{check_id}`"))?,
        ),
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

fn toml_parse_diagnostic(
    relative_path: &Path,
    contents: &str,
    err: toml::de::Error,
) -> ConfigDiagnostic {
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

fn config_check_diagnostic(
    check_id: String,
    source_path: PathBuf,
    message: String,
) -> ConfigDiagnostic {
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
    ConfigDiagnostic {
        check_id,
        message,
        location: Location { path, line, column },
        remediation,
    }
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
