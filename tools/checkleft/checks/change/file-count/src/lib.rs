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
//! A one-off exception uses `BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>` in
//! the commit or PR description. The bypass is bound to that approved
//! maximum: the check passes while the non-deleted file count is ≤ it, and
//! fails again if the change grows past it. A directive without a numeric
//! maximum is rejected (not treated as unlimited). When several directives
//! are present, the most recent one wins.
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

/// Bypass directive for this check. Value shape: `<max_files>:<reason>`.
const BYPASS_DIRECTIVE_NAME: &str = "BYPASS_CHANGE_FILE_COUNT";

/// Split a large change so it stays reviewable.
const SPLIT_REMEDIATION: &str = "Break the work into smaller tasks/PRs so each change stays reviewable and within the file-count limit.";

/// How to request a bound `change/file-count` exception.
const BYPASS_REMEDIATION: &str = "If this large surface is intentional (coordinated rename, generated tree that cannot split, etc.), request a one-off exception with `BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>` in the commit description (read in every CI context) or the PR description (best-effort). The numeric maximum is the approved ceiling; growth past it needs a new approval or a split.";

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

/// Latest `BYPASS_CHANGE_FILE_COUNT` value in commit then PR description.
///
/// Later lines win; a PR directive wins over commit directives. Empty values
/// are skipped the same way the host's generic bypass parser skips them.
fn latest_file_count_bypass_raw(commit_description: Option<&str>, pr_description: Option<&str>) -> Option<String> {
    let mut last = None;
    for text in [commit_description, pr_description].into_iter().flatten() {
        for line in text.lines() {
            if let Some(raw) = file_count_bypass_raw_from_line(line) {
                last = Some(raw);
            }
        }
    }
    last
}

fn file_count_bypass_raw_from_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with("BYPASS_") {
        return None;
    }
    let (raw_name, raw_value) = trimmed.split_once('=')?;
    if raw_name.trim().eq_ignore_ascii_case(BYPASS_DIRECTIVE_NAME) {
        let value = raw_value.trim();
        if value.is_empty() {
            return None;
        }
        return Some(value.to_owned());
    }
    None
}

/// Parse `BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>`.
///
/// `max_files` must be a decimal `usize`. The reason is the rest of the value
/// after the first colon, trimmed; it must be non-empty. A value with no
/// colon, a non-numeric maximum, or an empty reason is invalid.
fn parse_approved_file_count_bypass(raw: &str) -> Option<(usize, String)> {
    let (max_part, reason_part) = raw.split_once(':')?;
    let max_files = max_part.trim().parse().ok()?;
    let reason = reason_part.trim();
    if reason.is_empty() {
        return None;
    }
    Some((max_files, reason.to_owned()))
}

fn exceeded_configured_max_finding(count: usize, max_files: usize) -> Finding {
    Finding::error(format!(
        "changeset touches {count} non-deleted files, exceeding configured max_files={max_files}"
    ))
    .with_remediation(SPLIT_REMEDIATION.to_owned())
    .with_remediation(BYPASS_REMEDIATION.to_owned())
}

fn exceeded_approved_max_finding(count: usize, approved_max: usize) -> Finding {
    Finding::error(format!(
        "changeset touches {count} non-deleted files, exceeding the approved BYPASS_CHANGE_FILE_COUNT maximum of {approved_max}"
    ))
    .with_remediation(SPLIT_REMEDIATION.to_owned())
    .with_remediation(format!(
        "The previous exception was bound to {approved_max} files (current count {count}). Request a new approval with `BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>` or split the change."
    ))
}

fn malformed_file_count_bypass_finding(raw: &str) -> Finding {
    Finding::error(format!(
        "BYPASS_CHANGE_FILE_COUNT requires an approved maximum: `BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>` (got `{raw}`). A bypass without a numeric maximum is not treated as unlimited."
    ))
    .with_remediation(
        "Replace the directive with `BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>`, where max_files is the approved ceiling for this change.".to_owned(),
    )
}

/// Findings once the configured `max_files` ceiling has already been exceeded.
fn findings_for_oversized_change(
    count: usize,
    max_files: usize,
    commit_description: Option<&str>,
    pr_description: Option<&str>,
) -> Vec<Finding> {
    match latest_file_count_bypass_raw(commit_description, pr_description) {
        None => vec![exceeded_configured_max_finding(count, max_files)],
        Some(raw) => match parse_approved_file_count_bypass(&raw) {
            Some((approved_max, _)) if count <= approved_max => Vec::new(),
            Some((approved_max, _)) => vec![exceeded_approved_max_finding(count, approved_max)],
            None => vec![malformed_file_count_bypass_finding(&raw)],
        },
    }
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

    findings_for_oversized_change(
        count,
        max_files,
        input.changeset.commit_description.as_deref(),
        input.changeset.pr_description.as_deref(),
    )
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
        make_input_with_descriptions(files, config_json, None, None)
    }

    fn make_input_with_descriptions(
        files: Vec<ChangedFile>,
        config_json: &str,
        commit_description: Option<&str>,
        pr_description: Option<&str>,
    ) -> CheckInput {
        CheckInput::__from_parts(
            ChangeSet {
                changed_files: files,
                file_diffs: vec![],
                commit_description: commit_description.map(str::to_owned),
                pr_description: pr_description.map(str::to_owned),
                pr_description_unavailable_reason: None,
                change_id: None,
                repository: None,
                base_files: vec![],
                whole_repo: false,
            },
            config_json.to_owned(),
        )
    }

    fn oversized_files(n: usize) -> Vec<ChangedFile> {
        (0..n).map(|i| file(&format!("f{i}.rs"), ChangeKind::Added)).collect()
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
                .any(|r| r.contains("BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>")),
            "remediation should mention bound bypass; got {:?}",
            findings[0].remediations
        );
        assert!(findings[0].location.is_none());
    }

    #[test]
    fn bypass_within_approved_max_passes() {
        let findings = change_file_count_check(make_input_with_descriptions(
            oversized_files(6),
            r#"{"max_files": 5}"#,
            Some("BYPASS_CHANGE_FILE_COUNT=8:Coordinated rename of the protocol surface."),
            None,
        ));
        assert!(
            findings.is_empty(),
            "count 6 is within approved max 8; findings: {:?}",
            findings
        );
    }

    #[test]
    fn bypass_above_approved_max_fails_with_counts() {
        let findings = change_file_count_check(make_input_with_descriptions(
            oversized_files(10),
            r#"{"max_files": 5}"#,
            Some("BYPASS_CHANGE_FILE_COUNT=8:Coordinated rename of the protocol surface."),
            None,
        ));

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, checkleft_check_sdk::Severity::Error);
        assert!(
            findings[0].message.contains("10 non-deleted files")
                && findings[0].message.contains("approved")
                && findings[0].message.contains('8'),
            "message was: {}",
            findings[0].message
        );
        assert!(
            findings[0]
                .remediations
                .iter()
                .any(|r| r.contains("new approval") || r.contains("split")),
            "remediation should require a new approval or a split; got {:?}",
            findings[0].remediations
        );
    }

    #[test]
    fn bypass_missing_numeric_max_is_rejected() {
        let findings = change_file_count_check(make_input_with_descriptions(
            oversized_files(6),
            r#"{"max_files": 5}"#,
            Some("BYPASS_CHANGE_FILE_COUNT=Coordinated rename of the protocol surface."),
            None,
        ));

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, checkleft_check_sdk::Severity::Error);
        assert!(
            findings[0].message.contains("approved maximum")
                && findings[0].message.contains("<max_files>:<reason>")
                && findings[0].message.contains("not treated as unlimited"),
            "message was: {}",
            findings[0].message
        );
    }

    #[test]
    fn bypass_non_numeric_max_is_rejected() {
        let findings = change_file_count_check(make_input_with_descriptions(
            oversized_files(6),
            r#"{"max_files": 5}"#,
            Some("BYPASS_CHANGE_FILE_COUNT=many:Coordinated rename of the protocol surface."),
            None,
        ));

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("approved maximum") && findings[0].message.contains("many:"),
            "message was: {}",
            findings[0].message
        );
    }

    #[test]
    fn latest_bypass_directive_wins() {
        let commit = "\
BYPASS_CHANGE_FILE_COUNT=6:first approval, too small once the change grew.
BYPASS_CHANGE_FILE_COUNT=12:later approval after the surface was re-counted.
";
        let findings = change_file_count_check(make_input_with_descriptions(
            oversized_files(10),
            r#"{"max_files": 5}"#,
            Some(commit),
            None,
        ));
        assert!(
            findings.is_empty(),
            "latest directive approves 12; findings: {:?}",
            findings
        );

        let older_last = "\
BYPASS_CHANGE_FILE_COUNT=12:earlier larger approval.
BYPASS_CHANGE_FILE_COUNT=6:later tighter approval.
";
        let findings = change_file_count_check(make_input_with_descriptions(
            oversized_files(10),
            r#"{"max_files": 5}"#,
            Some(older_last),
            None,
        ));
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("10 non-deleted files") && findings[0].message.contains('6'),
            "latest directive of 6 must bind; message was: {}",
            findings[0].message
        );
    }

    #[test]
    fn latest_bypass_directive_prefers_pr_description() {
        let findings = change_file_count_check(make_input_with_descriptions(
            oversized_files(10),
            r#"{"max_files": 5}"#,
            Some("BYPASS_CHANGE_FILE_COUNT=12:from commit."),
            Some("BYPASS_CHANGE_FILE_COUNT=6:from PR."),
        ));
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains('6'),
            "PR description is the later surface; message was: {}",
            findings[0].message
        );
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
