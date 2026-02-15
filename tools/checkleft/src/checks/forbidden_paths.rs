use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::check::Check;
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

#[derive(Debug, Default)]
pub struct ForbiddenPathsCheck;

#[async_trait]
impl Check for ForbiddenPathsCheck {
    fn id(&self) -> &str {
        "forbidden-paths"
    }

    fn description(&self) -> &str {
        "flags changed files whose paths match forbidden glob patterns"
    }

    async fn run(
        &self,
        changeset: &ChangeSet,
        _tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let compiled = parse_config(config)?;
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if let Some(exclude_globs) = &compiled.exclude_globs {
                if exclude_globs.is_match(&changed_file.path) {
                    continue;
                }
            }

            let matches = compiled.patterns.matches(&changed_file.path);
            if matches.is_empty() {
                continue;
            }
            let matched_pattern = &compiled.pattern_strings[matches[0]];

            findings.push(Finding {
                severity: compiled.severity,
                message: format!("path matches forbidden pattern `{matched_pattern}`"),
                location: Some(Location {
                    path: changed_file.path.clone(),
                    line: None,
                    column: None,
                }),
                remediation: Some(compiled.remediation.clone()),
                suggested_fix: None,
            });
        }

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ForbiddenPathsConfig {
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    exclude_globs: Vec<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    remediation: Option<String>,
}

struct CompiledForbiddenPathsConfig {
    pattern_strings: Vec<String>,
    patterns: GlobSet,
    exclude_globs: Option<GlobSet>,
    severity: Severity,
    remediation: String,
}

fn parse_config(config: &toml::Value) -> Result<CompiledForbiddenPathsConfig> {
    let parsed: ForbiddenPathsConfig = config
        .clone()
        .try_into()
        .context("invalid forbidden-paths check config")?;

    if parsed.patterns.is_empty() {
        bail!("forbidden-paths check config must contain at least one `patterns` entry");
    }

    let patterns = compile_globset("patterns", &parsed.patterns)?;
    let exclude_globs = if parsed.exclude_globs.is_empty() {
        None
    } else {
        Some(compile_globset("exclude_globs", &parsed.exclude_globs)?)
    };

    Ok(CompiledForbiddenPathsConfig {
        pattern_strings: parsed.patterns,
        patterns,
        exclude_globs,
        severity: Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error),
        remediation: parsed.remediation.unwrap_or_else(|| {
            "Do not commit generated output artifacts. Remove this path from versioned changes."
                .to_owned()
        }),
    })
}

fn compile_globset(field_name: &str, patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .with_context(|| format!("invalid `{field_name}` glob pattern: {pattern}"))?;
        builder.add(glob);
    }

    builder
        .build()
        .with_context(|| format!("failed to compile `{field_name}` glob patterns"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::Severity;
    use crate::source_tree::LocalSourceTree;

    use super::ForbiddenPathsCheck;

    #[tokio::test]
    async fn flags_forbidden_output_path() {
        let temp = tempdir().expect("create temp dir");
        let artifact = temp.path().join("mobile/ios/.build/workspace-state.json");
        fs::create_dir_all(artifact.parent().expect("artifact parent")).expect("create dirs");
        fs::write(&artifact, "{}").expect("write artifact");

        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("mobile/ios/.build/workspace-state.json").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    patterns = ["**/.build/**"]
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].severity, Severity::Error);
        assert!(result.findings[0].message.contains("**/.build/**"));
    }

    #[tokio::test]
    async fn ignores_deleted_files() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("mobile/ios/.build/workspace-state.json").to_path_buf(),
                    kind: ChangeKind::Deleted,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    patterns = ["**/.build/**"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn excludes_configured_paths() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("mobile/ios/.build/workspace-state.json").to_path_buf(),
                    kind: ChangeKind::Added,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    patterns = ["**/.build/**"]
                    exclude_globs = ["mobile/ios/.build/**"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn requires_at_least_one_pattern() {
        let temp = tempdir().expect("create temp dir");
        let check = ForbiddenPathsCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("backend/src/lib.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await;

        assert!(result.is_err());
    }
}
