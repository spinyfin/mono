use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};

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
        "warns when files exceed configured line limits"
    }

    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        let max_lines = config
            .get("max_lines")
            .and_then(toml::Value::as_integer)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_MAX_LINES);

        let severity = Severity::parse_with_default(
            config.get("severity").and_then(toml::Value::as_str),
            Severity::Warning,
        );
        let exclude_globs = parse_exclude_globs(config)?;
        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if let Some(exclude_globs) = &exclude_globs {
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
            if line_count <= max_lines {
                continue;
            }

            findings.push(Finding {
                severity,
                message: format!(
                    "file has {line_count} lines, exceeding configured max_lines={max_lines}"
                ),
                location: Some(Location {
                    path: changed_file.path.clone(),
                    line: Some((max_lines.saturating_add(1)) as u32),
                    column: Some(1),
                }),
                remediation: Some(
                    "Split the file or refactor into smaller modules to reduce line count."
                        .to_owned(),
                ),
                suggested_fix: None,
            });
        }

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings,
        })
    }
}

fn parse_exclude_globs(config: &toml::Value) -> Result<Option<GlobSet>> {
    let Some(raw_patterns) = config.get("exclude_globs") else {
        return Ok(None);
    };

    let Some(patterns) = raw_patterns.as_array() else {
        bail!("`exclude_globs` must be an array of glob patterns");
    };
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let Some(pattern) = pattern.as_str() else {
            bail!("`exclude_globs` must contain only string patterns");
        };
        let glob = Glob::new(pattern)
            .with_context(|| format!("invalid `exclude_globs` pattern: {pattern}"))?;
        builder.add(glob);
    }

    let globset = builder
        .build()
        .context("failed to compile `exclude_globs` patterns")?;
    Ok(Some(globset))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
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
}
