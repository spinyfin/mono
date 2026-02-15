use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::bypass::{bypass_failure_guidance, bypass_name_for_check_id, maybe_bypass_findings};
use crate::check::Check;
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct ApiBreakingSurfaceCheck;

#[async_trait]
impl Check for ApiBreakingSurfaceCheck {
    fn id(&self) -> &str {
        "api-breaking-surface"
    }

    fn description(&self) -> &str {
        "requires API-facing backend changes to include configured documentation/version marker updates"
    }

    async fn run(
        &self,
        changeset: &ChangeSet,
        _tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let config = parse_config(config)?;
        let mut trigger_files = Vec::new();
        let mut required_updated = false;

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }

            if config.required_globs.is_match(&changed_file.path) {
                required_updated = true;
            }
            if config.trigger_globs.is_match(&changed_file.path) {
                trigger_files.push(changed_file.path.clone());
            }
        }

        if trigger_files.is_empty() || required_updated {
            return Ok(CheckResult {
                check_id: self.id().to_owned(),
                findings: Vec::new(),
            });
        }

        if let Some(findings) = maybe_bypass_findings(
            changeset,
            config.allow_bypass,
            &config.bypass_name,
            &trigger_files,
        ) {
            return Ok(CheckResult {
                check_id: self.id().to_owned(),
                findings,
            });
        }

        let remediation = if config.allow_bypass {
            format!(
                "{} {}",
                config.remediation,
                bypass_failure_guidance(&config.bypass_name)
            )
        } else {
            config.remediation.clone()
        };

        let findings = trigger_files
            .into_iter()
            .map(|path| Finding {
                severity: config.severity,
                message: config.message.clone(),
                location: Some(Location {
                    path,
                    line: None,
                    column: None,
                }),
                remediation: Some(remediation.clone()),
                suggested_fix: None,
            })
            .collect();

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ApiBreakingSurfaceConfig {
    #[serde(default)]
    trigger_globs: Vec<String>,
    #[serde(default)]
    required_globs: Vec<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    allow_bypass: Option<bool>,
}

struct CompiledApiBreakingSurfaceConfig {
    trigger_globs: GlobSet,
    required_globs: GlobSet,
    message: String,
    remediation: String,
    severity: Severity,
    allow_bypass: bool,
    bypass_name: String,
}

fn parse_config(config: &toml::Value) -> Result<CompiledApiBreakingSurfaceConfig> {
    let parsed: ApiBreakingSurfaceConfig = config
        .clone()
        .try_into()
        .context("invalid api-breaking-surface config")?;

    if parsed.trigger_globs.is_empty() {
        bail!("api-breaking-surface config must define `trigger_globs`");
    }
    if parsed.required_globs.is_empty() {
        bail!("api-breaking-surface config must define `required_globs`");
    }

    Ok(CompiledApiBreakingSurfaceConfig {
        trigger_globs: compile_globs("trigger_globs", &parsed.trigger_globs)?,
        required_globs: compile_globs("required_globs", &parsed.required_globs)?,
        message: parsed.message.unwrap_or_else(|| {
            "backend API surface changed without required changelog/version marker update"
                .to_owned()
        }),
        remediation: parsed.remediation.unwrap_or_else(|| {
            "Update the configured companion docs/version marker files in this change.".to_owned()
        }),
        severity: Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error),
        allow_bypass: parsed.allow_bypass.unwrap_or(false),
        bypass_name: bypass_name_for_check_id("api-breaking-surface"),
    })
}

fn compile_globs(field_name: &str, patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .with_context(|| format!("invalid `{field_name}` glob pattern: {pattern}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .with_context(|| format!("failed to compile `{field_name}` globs"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::Severity;
    use crate::source_tree::LocalSourceTree;
    use tempfile::tempdir;

    use super::ApiBreakingSurfaceCheck;

    #[tokio::test]
    async fn flags_trigger_change_without_required_update() {
        let temp = tempdir().expect("create temp dir");
        let check = ApiBreakingSurfaceCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/blob/src/v3/auth.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    trigger_globs = ["backend/blob/src/v3/**"]
                    required_globs = ["docs/backend.md"]
                    allow_bypass = true
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }

    #[tokio::test]
    async fn passes_when_required_file_is_updated() {
        let temp = tempdir().expect("create temp dir");
        let check = ApiBreakingSurfaceCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![
                    ChangedFile {
                        path: Path::new("backend/blob/src/v3/auth.rs").to_path_buf(),
                        kind: ChangeKind::Modified,
                        old_path: None,
                    },
                    ChangedFile {
                        path: Path::new("docs/backend.md").to_path_buf(),
                        kind: ChangeKind::Modified,
                        old_path: None,
                    },
                ]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    trigger_globs = ["backend/blob/src/v3/**"]
                    required_globs = ["docs/backend.md"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn ignores_backend_changes_outside_trigger_globs() {
        let temp = tempdir().expect("create temp dir");
        let check = ApiBreakingSurfaceCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/blob/src/v2/fencer.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    trigger_globs = [
                        "backend/blob/src/app.rs",
                        "backend/blob/src/v2/mod.rs",
                        "backend/blob/src/v2/model.rs",
                    ]
                    required_globs = ["docs/backend.md"]
                    allow_bypass = true
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn emits_warning_when_bypass_directive_exists_and_bypass_is_enabled() {
        let temp = tempdir().expect("create temp dir");
        let check = ApiBreakingSurfaceCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: Path::new("backend/blob/src/v3/auth.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_API_BREAKING_SURFACE=No public API behavior changed.".to_owned(),
        ));

        let result = check
            .run(
                &changeset,
                &tree,
                &toml::Value::Table(toml::toml! {
                    trigger_globs = ["backend/blob/src/v3/**"]
                    required_globs = ["docs/backend.md"]
                    allow_bypass = true
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].severity, Severity::Warning);
        assert!(
            result.findings[0]
                .message
                .contains("BYPASS_API_BREAKING_SURFACE")
        );
        assert!(
            result.findings[0]
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("No public API behavior changed.")
        );
    }

    #[tokio::test]
    async fn includes_bypass_guidance_when_bypass_is_enabled_and_missing() {
        let temp = tempdir().expect("create temp dir");
        let check = ApiBreakingSurfaceCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");

        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/blob/src/v3/auth.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    trigger_globs = ["backend/blob/src/v3/**"]
                    required_globs = ["docs/backend.md"]
                    allow_bypass = true
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        let remediation = result.findings[0]
            .remediation
            .as_ref()
            .expect("remediation present");
        assert!(remediation.contains("BYPASS_API_BREAKING_SURFACE"));
        assert!(remediation.contains("Never use bypasses for convenience"));
    }

    #[tokio::test]
    async fn does_not_bypass_when_bypass_is_disabled() {
        let temp = tempdir().expect("create temp dir");
        let check = ApiBreakingSurfaceCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: Path::new("backend/blob/src/v3/auth.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_API_BREAKING_SURFACE=No public API behavior changed.".to_owned(),
        ));

        let result = check
            .run(
                &changeset,
                &tree,
                &toml::Value::Table(toml::toml! {
                    trigger_globs = ["backend/blob/src/v3/**"]
                    required_globs = ["docs/backend.md"]
                    allow_bypass = false
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
    }
}
