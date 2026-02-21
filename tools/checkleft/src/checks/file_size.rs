use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::bypass::{bypass_failure_guidance, bypass_name_for_check_id, maybe_bypass_findings};
use crate::check::Check;
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

const DEFAULT_MAX_LINES: usize = 500;

#[derive(Debug, Default)]
pub struct FileSizeCheck;

#[async_trait]
impl Check for FileSizeCheck {
    fn id(&self) -> &str {
        "file-size"
    }

    fn description(&self) -> &str {
        "flags files exceeding configured line limits"
    }

    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let config = parse_config(config)?;
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if let Some(exclude_globs) = &config.exclude_globs {
                if exclude_globs.is_match(&changed_file.path) {
                    continue;
                }
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };
            let Ok(contents) = std::str::from_utf8(&contents) else {
                continue;
            };

            let line_count = contents.lines().count();
            if line_count <= config.max_lines {
                continue;
            }

            findings.push(Finding {
                severity: config.severity,
                message: format!(
                    "file has {line_count} lines, exceeding configured max_lines={}",
                    config.max_lines
                ),
                location: Some(Location {
                    path: changed_file.path.clone(),
                    line: Some((config.max_lines.saturating_add(1)) as u32),
                    column: Some(1),
                }),
                remediation: Some(
                    "Split the file or refactor into smaller modules to reduce line count."
                        .to_owned(),
                ),
                suggested_fix: None,
            });
        }

        if !findings.is_empty() {
            let trigger_files = finding_paths(&findings);
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
        }

        if config.allow_bypass && !findings.is_empty() {
            let guidance = bypass_failure_guidance(&config.bypass_name);
            for finding in &mut findings {
                finding.remediation = Some(match finding.remediation.take() {
                    Some(remediation) => format!("{remediation} {guidance}"),
                    None => guidance.clone(),
                });
            }
        }

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct FileSizeConfig {
    #[serde(default)]
    max_lines: Option<i64>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    exclude_globs: Option<Vec<String>>,
    #[serde(default)]
    allow_bypass: Option<bool>,
    #[serde(default)]
    bypass_name: Option<String>,
}

struct ParsedFileSizeConfig {
    max_lines: usize,
    severity: Severity,
    exclude_globs: Option<GlobSet>,
    allow_bypass: bool,
    bypass_name: String,
}

fn parse_config(config: &toml::Value) -> Result<ParsedFileSizeConfig> {
    let parsed: FileSizeConfig = config
        .clone()
        .try_into()
        .context("invalid file-size check config")?;

    let max_lines = match parsed.max_lines {
        Some(value) => usize::try_from(value).context("`max_lines` must be a non-negative integer")?,
        None => DEFAULT_MAX_LINES,
    };

    Ok(ParsedFileSizeConfig {
        max_lines,
        severity: Severity::parse_with_default(parsed.severity.as_deref(), Severity::Warning),
        exclude_globs: parse_exclude_globs(parsed.exclude_globs.as_deref())?,
        allow_bypass: parsed.allow_bypass.unwrap_or(false),
        bypass_name: resolve_bypass_name(parsed.bypass_name),
    })
}

fn parse_exclude_globs(patterns: Option<&[String]>) -> Result<Option<GlobSet>> {
    let Some(patterns) = patterns else {
        return Ok(None);
    };
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .with_context(|| format!("invalid `exclude_globs` pattern: {pattern}"))?;
        builder.add(glob);
    }

    let globset = builder
        .build()
        .context("failed to compile `exclude_globs` patterns")?;
    Ok(Some(globset))
}

fn finding_paths(findings: &[Finding]) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for finding in findings {
        if let Some(location) = finding.location.as_ref() {
            paths.insert(location.path.clone());
        }
    }

    paths.into_iter().collect()
}

fn resolve_bypass_name(config_bypass_name: Option<String>) -> String {
    let Some(name) = config_bypass_name else {
        return bypass_name_for_check_id("file-size");
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return bypass_name_for_check_id("file-size");
    }
    if trimmed.to_ascii_uppercase().starts_with("BYPASS_") {
        return trimmed.to_ascii_uppercase();
    }

    bypass_name_for_check_id(trimmed)
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

    use super::FileSizeCheck;

    #[tokio::test]
    async fn flags_files_over_limit() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("big.rs"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("big.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! { max_lines = 2 }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(result.findings[0].message.contains("max_lines=2"));
    }

    #[tokio::test]
    async fn ignores_files_within_limit() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("small.rs"), "a\nb\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("small.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! { max_lines = 5 }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn excludes_configured_paths() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("package-lock.json"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("package-lock.json").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    max_lines = 2
                    exclude_globs = ["**/package-lock.json"]
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn includes_bypass_guidance_when_enabled_and_missing() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("big.rs"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("big.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    max_lines = 2
                    allow_bypass = true
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(
            result.findings[0]
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("BYPASS_FILE_SIZE=<specific legitimate reason>")
        );
    }

    #[tokio::test]
    async fn emits_warning_when_bypass_directive_exists_and_bypass_is_enabled() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("big.rs"), "a\nb\nc\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: Path::new("big.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_FILE_SIZE=Generated file is mirrored from upstream.".to_owned(),
        ));
        let result = check
            .run(
                &changeset,
                &tree,
                &toml::Value::Table(toml::toml! {
                    max_lines = 2
                    severity = "error"
                    allow_bypass = true
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].severity, Severity::Warning);
        assert!(result.findings[0].message.contains("BYPASS_FILE_SIZE"));
        assert!(
            result.findings[0]
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("Generated file is mirrored from upstream.")
        );
    }

    #[tokio::test]
    async fn does_not_emit_bypass_warning_without_violations() {
        let temp = tempdir().expect("create temp dir");
        fs::write(temp.path().join("small.rs"), "a\nb\n").expect("write file");

        let check = FileSizeCheck;
        let tree = LocalSourceTree::new(temp.path()).expect("create tree");
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: Path::new("small.rs").to_path_buf(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_FILE_SIZE=Only needed if this check fails.".to_owned(),
        ));
        let result = check
            .run(
                &changeset,
                &tree,
                &toml::Value::Table(toml::toml! {
                    max_lines = 5
                    allow_bypass = true
                }),
            )
            .await
            .expect("run check");

        assert!(result.findings.is_empty());
    }
}
