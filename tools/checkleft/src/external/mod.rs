use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::path::validate_relative_path;

pub const EXTERNAL_CHECK_RUNTIME_V1: &str = "sandbox-v1";
pub const EXTERNAL_CHECK_API_V1: &str = "v1";
pub const GENERATED_IMPLEMENTATION_PREFIX: &str = "generated:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCheckImplementationRef {
    File(PathBuf),
    Generated(String),
}

impl ExternalCheckImplementationRef {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("implementation reference must not be empty");
        }

        if let Some(generated_id) = trimmed.strip_prefix(GENERATED_IMPLEMENTATION_PREFIX) {
            let generated_id = generated_id.trim();
            if generated_id.is_empty() {
                bail!(
                    "generated implementation reference must include an id after `{}`",
                    GENERATED_IMPLEMENTATION_PREFIX
                );
            }
            return Ok(Self::Generated(generated_id.to_owned()));
        }

        let path = PathBuf::from(trimmed);
        validate_relative_path(&path)?;
        Ok(Self::File(path))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckPackage {
    pub id: String,
    pub runtime: String,
    pub api_version: String,
    pub capabilities: ExternalCheckCapabilities,
    pub implementation: ExternalCheckPackageImplementation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCheckPackageImplementation {
    Source(ExternalCheckSourcePackage),
    Artifact(ExternalCheckArtifactPackage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckSourcePackage {
    pub language: String,
    pub entry: String,
    pub build_adapter: String,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckArtifactPackage {
    pub artifact_path: String,
    pub artifact_sha256: String,
    pub provenance: Option<ExternalCheckArtifactProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCheckArtifactProvenance {
    pub generator: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExternalCheckCapabilities {
    pub commands: Vec<String>,
}

pub trait ExternalCheckPackageProvider: Send + Sync {
    fn resolve(
        &self,
        implementation_ref: &ExternalCheckImplementationRef,
    ) -> Result<Option<ExternalCheckPackage>>;
}

pub fn load_external_check_package_manifest(path: &Path) -> Result<ExternalCheckPackage> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read external check manifest {}", path.display()))?;
    parse_external_check_package_manifest(&contents)
        .with_context(|| format!("invalid external check manifest {}", path.display()))
}

pub fn parse_external_check_package_manifest(contents: &str) -> Result<ExternalCheckPackage> {
    let raw: RawExternalCheckPackage =
        toml::from_str(contents).context("failed to parse external check manifest TOML")?;
    raw.validate()
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawExternalCheckMode {
    Source,
    Artifact,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExternalCheckPackage {
    id: String,
    runtime: String,
    api_version: String,
    mode: RawExternalCheckMode,
    #[serde(default)]
    capabilities: RawExternalCheckCapabilities,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    build_adapter: Option<String>,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    artifact_path: Option<String>,
    #[serde(default)]
    artifact_sha256: Option<String>,
    #[serde(default)]
    provenance: Option<ExternalCheckArtifactProvenance>,
}

impl RawExternalCheckPackage {
    fn validate(self) -> Result<ExternalCheckPackage> {
        let RawExternalCheckPackage {
            id,
            runtime,
            api_version,
            mode,
            capabilities,
            language,
            entry,
            build_adapter,
            sources,
            artifact_path,
            artifact_sha256,
            provenance,
        } = self;

        let id = required_non_empty("id", id)?;
        let runtime = required_non_empty("runtime", runtime)?;
        if runtime != EXTERNAL_CHECK_RUNTIME_V1 {
            bail!(
                "unsupported runtime `{runtime}` (expected `{}`)",
                EXTERNAL_CHECK_RUNTIME_V1
            );
        }

        let api_version = required_non_empty("api_version", api_version)?;
        if api_version != EXTERNAL_CHECK_API_V1 {
            bail!(
                "unsupported api_version `{api_version}` (expected `{}`)",
                EXTERNAL_CHECK_API_V1
            );
        }

        let capabilities = capabilities.validate()?;
        let implementation = match mode {
            RawExternalCheckMode::Source => {
                ExternalCheckPackageImplementation::Source(validate_source_implementation(
                    language,
                    entry,
                    build_adapter,
                    sources,
                    artifact_path,
                    artifact_sha256,
                    provenance,
                )?)
            }
            RawExternalCheckMode::Artifact => {
                ExternalCheckPackageImplementation::Artifact(validate_artifact_implementation(
                    language,
                    entry,
                    build_adapter,
                    sources,
                    artifact_path,
                    artifact_sha256,
                    provenance,
                )?)
            }
        };

        Ok(ExternalCheckPackage {
            id,
            runtime,
            api_version,
            capabilities,
            implementation,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_source_implementation(
    language: Option<String>,
    entry: Option<String>,
    build_adapter: Option<String>,
    sources: Vec<String>,
    artifact_path: Option<String>,
    artifact_sha256: Option<String>,
    provenance: Option<ExternalCheckArtifactProvenance>,
) -> Result<ExternalCheckSourcePackage> {
    reject_if_present("artifact_path", artifact_path.as_ref())?;
    reject_if_present("artifact_sha256", artifact_sha256.as_ref())?;
    reject_if_present("provenance", provenance.as_ref())?;

    let sources = sources
        .into_iter()
        .map(|source| required_relative_path_string("sources[]", source))
        .collect::<Result<Vec<_>>>()?;

    Ok(ExternalCheckSourcePackage {
        language: required_some_non_empty("language", language)?,
        entry: required_some_relative_path_string("entry", entry)?,
        build_adapter: required_some_non_empty("build_adapter", build_adapter)?,
        sources,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_artifact_implementation(
    language: Option<String>,
    entry: Option<String>,
    build_adapter: Option<String>,
    sources: Vec<String>,
    artifact_path: Option<String>,
    artifact_sha256: Option<String>,
    provenance: Option<ExternalCheckArtifactProvenance>,
) -> Result<ExternalCheckArtifactPackage> {
    reject_if_present("language", language.as_ref())?;
    reject_if_present("entry", entry.as_ref())?;
    reject_if_present("build_adapter", build_adapter.as_ref())?;
    if !sources.is_empty() {
        bail!("field `sources` is not allowed in `artifact` mode");
    }

    Ok(ExternalCheckArtifactPackage {
        artifact_path: required_some_relative_path_string("artifact_path", artifact_path)?,
        artifact_sha256: required_some_sha256("artifact_sha256", artifact_sha256)?,
        provenance,
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExternalCheckCapabilities {
    #[serde(default)]
    commands: Vec<String>,
}

impl RawExternalCheckCapabilities {
    fn validate(&self) -> Result<ExternalCheckCapabilities> {
        let mut seen = HashSet::new();
        let mut commands = Vec::with_capacity(self.commands.len());

        for command in &self.commands {
            let command = required_non_empty("capabilities.commands[]", command.clone())?;
            if command.contains('/') || command.contains('\\') {
                bail!(
                    "command `{command}` must be a bare command name, not a path in `capabilities.commands`"
                );
            }
            if command.chars().any(char::is_whitespace) {
                bail!("command `{command}` must not contain whitespace");
            }
            if !seen.insert(command.clone()) {
                bail!("duplicate command `{command}` in `capabilities.commands`");
            }
            commands.push(command);
        }

        Ok(ExternalCheckCapabilities { commands })
    }
}

fn reject_if_present<T>(field_name: &str, value: Option<&T>) -> Result<()> {
    if value.is_some() {
        bail!("field `{field_name}` is not allowed for this package mode");
    }
    Ok(())
}

fn required_some_non_empty(field_name: &str, value: Option<String>) -> Result<String> {
    let Some(value) = value else {
        bail!("missing required field `{field_name}`");
    };
    required_non_empty(field_name, value)
}

fn required_some_relative_path_string(field_name: &str, value: Option<String>) -> Result<String> {
    let Some(value) = value else {
        bail!("missing required field `{field_name}`");
    };
    required_relative_path_string(field_name, value)
}

fn required_some_sha256(field_name: &str, value: Option<String>) -> Result<String> {
    let Some(value) = value else {
        bail!("missing required field `{field_name}`");
    };
    required_sha256(field_name, value)
}

fn required_non_empty(field_name: &str, value: String) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("field `{field_name}` must not be empty");
    }
    Ok(trimmed.to_owned())
}

fn required_sha256(field_name: &str, value: String) -> Result<String> {
    let normalized = required_non_empty(field_name, value)?;
    let is_valid = normalized.len() == 64
        && normalized
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !is_valid {
        bail!(
            "field `{field_name}` must be a canonical sha256 digest (64 lowercase hex characters)"
        );
    }
    Ok(normalized)
}

fn required_relative_path_string(field_name: &str, value: String) -> Result<String> {
    let normalized = required_non_empty(field_name, value)?;
    validate_relative_path(Path::new(&normalized))
        .with_context(|| format!("field `{field_name}` must be a safe relative path"))?;
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        EXTERNAL_CHECK_API_V1, EXTERNAL_CHECK_RUNTIME_V1, ExternalCheckImplementationRef,
        ExternalCheckPackageImplementation, parse_external_check_package_manifest,
    };

    #[test]
    fn parses_source_mode_manifest() {
        let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "source"
runtime = "sandbox-v1"
api_version = "v1"
language = "javascript"
entry = "./check.ts"
build_adapter = "javascript-component"
sources = ["./check.ts", "./package.json", "./pnpm-lock.yaml"]

[capabilities]
commands = ["grep", "sed"]
"#;

        let package = parse_external_check_package_manifest(manifest).expect("valid manifest");

        assert_eq!(package.id, "workflow-shell-strict-v2");
        assert_eq!(package.runtime, EXTERNAL_CHECK_RUNTIME_V1);
        assert_eq!(package.api_version, EXTERNAL_CHECK_API_V1);
        assert_eq!(package.capabilities.commands, vec!["grep", "sed"]);
        assert!(matches!(
            package.implementation,
            ExternalCheckPackageImplementation::Source(_)
        ));
    }

    #[test]
    fn parses_artifact_mode_manifest() {
        let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "artifact"
runtime = "sandbox-v1"
api_version = "v1"
artifact_path = "bazel-bin/checks/workflow_shell_strict/check.wasm"
artifact_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[provenance]
generator = "bazel"
target = "//checks/workflow_shell_strict:check_wasm"
"#;

        let package = parse_external_check_package_manifest(manifest).expect("valid manifest");
        assert!(matches!(
            package.implementation,
            ExternalCheckPackageImplementation::Artifact(_)
        ));
    }

    #[test]
    fn source_mode_rejects_artifact_fields() {
        let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "source"
runtime = "sandbox-v1"
api_version = "v1"
language = "javascript"
entry = "./check.ts"
build_adapter = "javascript-component"
artifact_path = "x.wasm"
artifact_sha256 = "abc"
"#;

        let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
        assert!(error.to_string().contains("artifact_path"));
    }

    #[test]
    fn artifact_mode_requires_required_fields() {
        let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "artifact"
runtime = "sandbox-v1"
api_version = "v1"
"#;

        let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
        assert!(error.to_string().contains("artifact_path"));
    }

    #[test]
    fn rejects_invalid_runtime() {
        let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "source"
runtime = "sandbox-v2"
api_version = "v1"
language = "javascript"
entry = "./check.ts"
build_adapter = "javascript-component"
"#;

        let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
        assert!(error.to_string().contains("unsupported runtime"));
    }

    #[test]
    fn rejects_duplicate_commands() {
        let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "source"
runtime = "sandbox-v1"
api_version = "v1"
language = "javascript"
entry = "./check.ts"
build_adapter = "javascript-component"

[capabilities]
commands = ["grep", "grep"]
"#;

        let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
        assert!(error.to_string().contains("duplicate command"));
    }

    #[test]
    fn rejects_unknown_manifest_fields() {
        let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "source"
runtime = "sandbox-v1"
api_version = "v1"
api_vesion = "v1"
language = "javascript"
entry = "./check.ts"
build_adapter = "javascript-component"
"#;

        let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
        let message = format!("{error:#}");
        assert!(message.contains("unknown field"));
        assert!(message.contains("api_vesion"));
    }

    #[test]
    fn rejects_non_canonical_artifact_sha256() {
        let manifest = r#"
id = "workflow-shell-strict-v2"
mode = "artifact"
runtime = "sandbox-v1"
api_version = "v1"
artifact_path = "bazel-bin/checks/workflow_shell_strict/check.wasm"
artifact_sha256 = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF"
"#;

        let error = parse_external_check_package_manifest(manifest).expect_err("must fail");
        assert!(error.to_string().contains("canonical sha256 digest"));
    }

    #[test]
    fn parses_generated_implementation_ref() {
        let implementation_ref =
            ExternalCheckImplementationRef::parse("generated:domain-typo-check")
                .expect("valid generated ref");
        assert!(matches!(
            implementation_ref,
            ExternalCheckImplementationRef::Generated(ref id) if id == "domain-typo-check"
        ));
    }

    #[test]
    fn parses_file_implementation_ref() {
        let implementation_ref =
            ExternalCheckImplementationRef::parse("checks/workflow-shell-strict/check.toml")
                .expect("valid file ref");
        assert_eq!(
            implementation_ref,
            ExternalCheckImplementationRef::File(PathBuf::from(
                "checks/workflow-shell-strict/check.toml"
            ))
        );
    }

    #[test]
    fn rejects_empty_generated_id() {
        let error = ExternalCheckImplementationRef::parse("generated:").expect_err("must fail");
        assert!(error.to_string().contains("include an id"));
    }

    #[test]
    fn rejects_absolute_file_implementation_ref() {
        let error = ExternalCheckImplementationRef::parse("/tmp/check.toml").expect_err("must fail");
        assert!(error.to_string().contains("absolute paths are not allowed"));
    }
}
