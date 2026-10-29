//! Checkleft check: flag changesets that touch too many non-deleted files.
//!
//! Registered under the canonical id `change/file-count`. Runs inside the
//! checkleft wasm host. It only counts the files already present in the
//! changeset the framework scheduled (the same change-detection universe every
//! other check sees), and emits a single locationless finding when that count
//! exceeds `max_files`.
//!
//! Deleted files do not count. The runner's files-scope scheduler already
//! omits `ChangeKind::Deleted` entries from per-check changesets; this check
//! also filters them explicitly so direct unit tests and future schedulers
//! stay consistent.
//!
//! Under `checkleft run --all` the host builds a whole-repo changeset
//! (`ChangeSet.whole_repo = true`) that lists every tracked file as Modified.
//! Counting that tree against `max_files` would always fail on any real
//! repository, so this check is a documented no-op in whole-repo mode — the
//! same class of caveat as `policy.changed_lines_only` under `--all`.
//!
//! ## Configuration (JSON-encoded, passed via `config-json`)
//!
//! ```json
//! {
//!   "max_files": 30
//! }
//! ```
//!
//! `max_files` defaults to [`DEFAULT_MAX_FILES`] (50) when omitted — a
//! conservative default for unconfigured consumers. Mono pins `30` from a
//! recent PR-size audit (just above p95 of the last 100 PRs).

use checkleft_check_sdk::{ChangeKind, CheckInput, Finding, check};
use serde::Deserialize;

/// Default ceiling when a CHECKS instance omits `max_files`.
///
/// Chosen as a generic upper bound for unconfigured consumers; mono's root
/// `CHECKS.yaml` overrides this to 30 based on a recent PR-size audit.
pub const DEFAULT_MAX_FILES: usize = 50;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    max_files: Option<u64>,
}

/// Count non-deleted changed files in the input changeset.
fn count_non_deleted_files(input: &CheckInput) -> usize {
    input
        .changeset
        .changed_files
        .iter()
        .filter(|f| f.kind != ChangeKind::Deleted)
        .count()
}

#[check(
    name = "change/file-count",
    description = "flags changesets that touch more non-deleted files than configured max_files",
    severity = error
)]
pub fn change_file_count_check(input: CheckInput) -> Vec<Finding> {
    // Integrity / `checkleft run --all` builds a whole-repo changeset that
    // marks every tracked file Modified. A max_files gate is only meaningful
    // for scoped PR/diff changes; under --all it would always fire on any
    // real tree. Same class of documented no-op as `changed_lines_only`.
    if input.changeset.whole_repo {
        return Vec::new();
    }

    let cfg: Config = input.config().unwrap_or_default();
    let max_files = cfg.max_files.map(|v| v as usize).unwrap_or(DEFAULT_MAX_FILES);
    let count = count_non_deleted_files(&input);

    if count <= max_files {
        return Vec::new();
    }

    vec![
        Finding::error(format!(
            "changeset touches {count} non-deleted files, exceeding configured max_files={max_files}"
        ))
        .with_remediation(
            "Break the work into smaller tasks/PRs so each change stays reviewable and within the file-count limit."
                .to_owned(),
        )
        .with_remediation(
            "If this large surface is intentional (coordinated rename, generated tree that cannot split, etc.), request a one-off exception with `BYPASS_CHANGE_FILE_COUNT=<specific legitimate reason>` in the commit description (read in every CI context) or the PR description (best-effort)."
                .to_owned(),
        ),
    ]
}

// NOTE: this crate is an rlib, NOT a standalone wasm component. The component
// ABI (`export_checks!` → `list-checks`/`run-check`) is wired ONCE in the
// aggregating `checkleft-preinstalled-bundle` crate, which links this check
// into a single multiplexed component.

#[cfg(test)]
mod tests {
    use super::*;
    use checkleft_check_sdk::{ChangeKind, ChangeSet, ChangedFile};

    fn file(path: &str, kind: ChangeKind) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            kind,
            old_path: None,
        }
    }

    fn make_input(files: Vec<ChangedFile>, config_json: &str) -> CheckInput {
        CheckInput::__from_parts(
            ChangeSet {
                changed_files: files,
                file_diffs: vec![],
                commit_description: None,
                pr_description: None,
                pr_description_unavailable_reason: None,
                change_id: None,
                repository: None,
                base_files: vec![],
                whole_repo: false,
            },
            config_json.to_owned(),
        )
    }

    fn make_whole_repo_input(files: Vec<ChangedFile>, config_json: &str) -> CheckInput {
        CheckInput::__from_parts(
            ChangeSet {
                changed_files: files,
                file_diffs: vec![],
                commit_description: None,
                pr_description: None,
                pr_description_unavailable_reason: None,
                change_id: None,
                repository: None,
                base_files: vec![],
                whole_repo: true,
            },
            config_json.to_owned(),
        )
    }

    #[test]
    fn count_skips_deleted_files() {
        let input = make_input(
            vec![
                file("a.rs", ChangeKind::Added),
                file("b.rs", ChangeKind::Modified),
                file("c.rs", ChangeKind::Deleted),
                file("d.rs", ChangeKind::Renamed),
            ],
            r#"{"max_files": 50}"#,
        );
        assert_eq!(count_non_deleted_files(&input), 3);
    }

    #[test]
    fn no_finding_when_count_at_or_below_max() {
        let files: Vec<_> = (0..5).map(|i| file(&format!("f{i}.rs"), ChangeKind::Added)).collect();
        let findings = change_file_count_check(make_input(files, r#"{"max_files": 5}"#));
        assert!(findings.is_empty());
    }

    #[test]
    fn one_finding_when_count_exceeds_max() {
        let files: Vec<_> = (0..6).map(|i| file(&format!("f{i}.rs"), ChangeKind::Added)).collect();
        let findings = change_file_count_check(make_input(files, r#"{"max_files": 5}"#));

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, checkleft_check_sdk::Severity::Error);
        assert!(
            findings[0].message.contains("6 non-deleted files") && findings[0].message.contains("max_files=5"),
            "message was: {}",
            findings[0].message
        );
        assert!(
            findings[0]
                .remediations
                .iter()
                .any(|r| r.contains("smaller tasks") || r.contains("smaller")),
            "remediation should recommend splitting; got {:?}",
            findings[0].remediations
        );
        assert!(
            findings[0]
                .remediations
                .iter()
                .any(|r| r.contains("BYPASS_CHANGE_FILE_COUNT")),
            "remediation should mention legitimate bypass; got {:?}",
            findings[0].remediations
        );
        assert!(findings[0].location.is_none());
    }

    #[test]
    fn deleted_only_files_do_not_inflate_count() {
        // 4 non-deleted + many deleted → still under max of 5.
        let mut files: Vec<_> = (0..4)
            .map(|i| file(&format!("keep{i}.rs"), ChangeKind::Modified))
            .collect();
        for i in 0..20 {
            files.push(file(&format!("gone{i}.rs"), ChangeKind::Deleted));
        }

        let findings = change_file_count_check(make_input(files, r#"{"max_files": 5}"#));
        assert!(
            findings.is_empty(),
            "deleted files must not inflate count; findings: {:?}",
            findings
        );
    }

    #[test]
    fn deleted_only_files_do_not_trigger_when_non_deleted_would() {
        // Without filtering deleted, this would fail max=2; with filtering it passes.
        let files = vec![
            file("a.rs", ChangeKind::Added),
            file("b.rs", ChangeKind::Deleted),
            file("c.rs", ChangeKind::Deleted),
            file("d.rs", ChangeKind::Deleted),
        ];
        let findings = change_file_count_check(make_input(files, r#"{"max_files": 2}"#));
        assert!(findings.is_empty());
    }

    #[test]
    fn default_max_files_is_fifty_when_unconfigured() {
        let under: Vec<_> = (0..DEFAULT_MAX_FILES)
            .map(|i| file(&format!("u{i}.rs"), ChangeKind::Added))
            .collect();
        let under_findings = change_file_count_check(make_input(under, "{}"));
        assert!(under_findings.is_empty());

        let over: Vec<_> = (0..=DEFAULT_MAX_FILES)
            .map(|i| file(&format!("o{i}.rs"), ChangeKind::Added))
            .collect();
        let over_findings = change_file_count_check(make_input(over, "{}"));
        assert_eq!(over_findings.len(), 1);
        assert!(
            over_findings[0]
                .message
                .contains(&format!("max_files={DEFAULT_MAX_FILES}")),
            "message was: {}",
            over_findings[0].message
        );
    }

    /// Synthetic stand-in for `Vcs::all_files_changeset` / `checkleft run --all`:
    /// every tracked path is Modified, count far exceeds max_files, but
    /// `whole_repo` makes the gate a no-op so integrity pipelines stay green.
    #[test]
    fn whole_repo_all_files_changeset_is_noop_even_when_count_exceeds_max() {
        let files: Vec<_> = (0..200)
            .map(|i| file(&format!("tracked{i}.rs"), ChangeKind::Modified))
            .collect();

        // Without the flag the same surface would hard-fail max_files=30.
        let fail = change_file_count_check(make_input(files.clone(), r#"{"max_files": 30}"#));
        assert_eq!(fail.len(), 1, "control: scoped oversized changeset must still fail");

        let findings = change_file_count_check(make_whole_repo_input(files, r#"{"max_files": 30}"#));
        assert!(
            findings.is_empty(),
            "whole_repo / --all changeset must no-op change/file-count; findings: {:?}",
            findings
        );
    }
}
