//! Checkleft check: flag changesets that touch too many non-deleted files.
//!
//! Registered under the canonical id `change/file-count`. This is a thin
//! built-in: it only counts the files already present in the changeset the
//! framework scheduled (the same change-detection universe every other check
//! sees), and emits a single locationless finding when that count exceeds
//! `max_files`.
//!
//! Deleted files do not count. The runner's files-scope scheduler already
//! omits `ChangeKind::Deleted` entries from per-check changesets; this check
//! also filters them explicitly so direct unit tests and future schedulers
//! stay consistent.
//!
//! ## Configuration
//!
//! ```toml
//! [[checks]]
//! id = "change/file-count"
//!
//! [checks.config]
//! max_files = 30
//!
//! [checks.policy]
//! severity = "error"
//! allow_bypass = true
//! ```
//!
//! `max_files` defaults to [`DEFAULT_MAX_FILES`] (50) when omitted — a
//! conservative default for unconfigured consumers. Mono pins `30` from a
//! recent PR-size audit (just above p95 of the last 100 PRs).

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Severity};

/// Default ceiling when a CHECKS instance omits `max_files`.
///
/// Chosen as a generic upper bound for unconfigured consumers; mono's root
/// `CHECKS.yaml` overrides this to 30 based on a recent PR-size audit.
pub const DEFAULT_MAX_FILES: usize = 50;

#[derive(Debug, Default)]
pub struct FileCountCheck;

#[async_trait]
impl Check for FileCountCheck {
    fn id(&self) -> &str {
        "change/file-count"
    }

    fn description(&self) -> &str {
        "flags changesets that touch more non-deleted files than configured max_files"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[derive(Debug, Deserialize, Default)]
struct FileCountConfig {
    #[serde(default)]
    max_files: Option<u64>,
}

struct CompiledFileCountConfig {
    max_files: usize,
}

fn parse_config(config: &toml::Value) -> Result<CompiledFileCountConfig> {
    let parsed: FileCountConfig = config.clone().try_into().context("invalid change/file-count config")?;
    Ok(CompiledFileCountConfig {
        max_files: parsed.max_files.map(|v| v as usize).unwrap_or(DEFAULT_MAX_FILES),
    })
}

/// Count non-deleted changed files in `changeset`.
pub fn count_non_deleted_files(changeset: &ChangeSet) -> usize {
    changeset
        .changed_files
        .iter()
        .filter(|f| !matches!(f.kind, ChangeKind::Deleted))
        .count()
}

#[async_trait]
impl ConfiguredCheck for CompiledFileCountConfig {
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        // One unit of work: the whole-changeset count, not a per-file scan.
        // Returning 1 keeps the progress UI honest for this non-iterating check.
        if changeset.changed_files.is_empty() { 0 } else { 1 }
    }

    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        _tree: &dyn SourceTree,
        on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        let count = count_non_deleted_files(changeset);
        on_file_processed(if count == 0 { 0 } else { 1 });

        let mut findings = Vec::new();
        if count > self.max_files {
            findings.push(Finding {
                fixable: false,
                severity: Severity::Error,
                message: format!(
                    "changeset touches {count} non-deleted files, exceeding configured max_files={}",
                    self.max_files
                ),
                location: None,
                surface: None,
                remediations: vec![
                    "Break the work into smaller tasks/PRs so each change stays reviewable and within the file-count limit."
                        .to_owned(),
                    "If this large surface is intentional (coordinated rename, generated tree that cannot split, etc.), request a one-off exception with `BYPASS_CHANGE_FILE_COUNT=<specific legitimate reason>` in the PR or commit description."
                        .to_owned(),
                ],
                suggested_fix: None,
            });
        }

        Ok(CheckResult {
            check_id: "change/file-count".to_owned(),
            findings,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{DEFAULT_MAX_FILES, FileCountCheck, count_non_deleted_files};
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::Severity;
    use crate::source_tree::LocalSourceTree;

    fn file(path: &str, kind: ChangeKind) -> ChangedFile {
        ChangedFile {
            path: Path::new(path).to_path_buf(),
            kind,
            old_path: None,
        }
    }

    fn config(max_files: u64) -> toml::Value {
        toml::from_str(&format!("max_files = {max_files}")).expect("valid config")
    }

    fn empty_config() -> toml::Value {
        toml::Value::Table(Default::default())
    }

    #[test]
    fn count_skips_deleted_files() {
        let changeset = ChangeSet::new(vec![
            file("a.rs", ChangeKind::Added),
            file("b.rs", ChangeKind::Modified),
            file("c.rs", ChangeKind::Deleted),
            file("d.rs", ChangeKind::Renamed),
        ]);
        assert_eq!(count_non_deleted_files(&changeset), 3);
    }

    #[tokio::test]
    async fn no_finding_when_count_at_or_below_max() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tree = LocalSourceTree::new(temp.path()).expect("tree");
        let check = FileCountCheck;

        let files: Vec<_> = (0..5).map(|i| file(&format!("f{i}.rs"), ChangeKind::Added)).collect();
        let result = check.run(&ChangeSet::new(files), &tree, &config(5)).await.expect("run");
        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn one_finding_when_count_exceeds_max() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tree = LocalSourceTree::new(temp.path()).expect("tree");
        let check = FileCountCheck;

        let files: Vec<_> = (0..6).map(|i| file(&format!("f{i}.rs"), ChangeKind::Added)).collect();
        let result = check.run(&ChangeSet::new(files), &tree, &config(5)).await.expect("run");

        assert_eq!(result.findings.len(), 1);
        let finding = &result.findings[0];
        assert_eq!(finding.severity, Severity::Error);
        assert!(
            finding.message.contains("6 non-deleted files") && finding.message.contains("max_files=5"),
            "message was: {}",
            finding.message
        );
        assert!(
            finding
                .remediations
                .iter()
                .any(|r| r.contains("smaller tasks") || r.contains("smaller")),
            "remediation should recommend splitting; got {:?}",
            finding.remediations
        );
        assert!(
            finding
                .remediations
                .iter()
                .any(|r| r.contains("BYPASS_CHANGE_FILE_COUNT")),
            "remediation should mention legitimate bypass; got {:?}",
            finding.remediations
        );
        assert!(finding.location.is_none());
    }

    #[tokio::test]
    async fn deleted_only_files_do_not_inflate_count() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tree = LocalSourceTree::new(temp.path()).expect("tree");
        let check = FileCountCheck;

        // 4 non-deleted + many deleted → still under max of 5.
        let mut files: Vec<_> = (0..4)
            .map(|i| file(&format!("keep{i}.rs"), ChangeKind::Modified))
            .collect();
        for i in 0..20 {
            files.push(file(&format!("gone{i}.rs"), ChangeKind::Deleted));
        }

        let result = check.run(&ChangeSet::new(files), &tree, &config(5)).await.expect("run");
        assert!(
            result.findings.is_empty(),
            "deleted files must not inflate count; findings: {:?}",
            result.findings
        );
    }

    #[tokio::test]
    async fn deleted_only_files_do_not_trigger_when_non_deleted_would() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tree = LocalSourceTree::new(temp.path()).expect("tree");
        let check = FileCountCheck;

        // Without filtering deleted, this would fail max=2; with filtering it passes.
        let files = vec![
            file("a.rs", ChangeKind::Added),
            file("b.rs", ChangeKind::Deleted),
            file("c.rs", ChangeKind::Deleted),
            file("d.rs", ChangeKind::Deleted),
        ];
        let result = check.run(&ChangeSet::new(files), &tree, &config(2)).await.expect("run");
        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn default_max_files_is_fifty_when_unconfigured() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tree = LocalSourceTree::new(temp.path()).expect("tree");
        let check = FileCountCheck;

        let under: Vec<_> = (0..DEFAULT_MAX_FILES)
            .map(|i| file(&format!("u{i}.rs"), ChangeKind::Added))
            .collect();
        let under_result = check
            .run(&ChangeSet::new(under), &tree, &empty_config())
            .await
            .expect("run");
        assert!(under_result.findings.is_empty());

        let over: Vec<_> = (0..=DEFAULT_MAX_FILES)
            .map(|i| file(&format!("o{i}.rs"), ChangeKind::Added))
            .collect();
        let over_result = check
            .run(&ChangeSet::new(over), &tree, &empty_config())
            .await
            .expect("run");
        assert_eq!(over_result.findings.len(), 1);
        assert!(
            over_result.findings[0]
                .message
                .contains(&format!("max_files={DEFAULT_MAX_FILES}")),
            "message was: {}",
            over_result.findings[0].message
        );
    }
}
