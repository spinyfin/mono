use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::input::{ChangeKind, ChangeSet};
use crate::output::{Finding, Location, Severity};

/// Parse BYPASS directives from free-form text.
///
/// Accepted format per line:
///   BYPASS_NAME=Reason text
///
/// Preserve explicit empty values: the latest declaration for a name supersedes
/// older ones even when it is empty or otherwise malformed. An empty latest
/// value revokes any earlier non-empty reason for that name; consumers decide
/// whether the retained value is valid.
pub fn parse_bypass_directives(text: &str) -> BTreeMap<String, String> {
    let mut directives = BTreeMap::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("BYPASS_") {
            continue;
        }

        let Some((raw_name, raw_reason)) = trimmed.split_once('=') else {
            continue;
        };

        let name = normalize_bypass_name(raw_name);
        if !is_valid_bypass_name(&name) {
            continue;
        }

        let reason = raw_reason.trim();

        directives.insert(name, reason.to_owned());
    }

    directives
}

pub fn parse_bypass_directives_from_descriptions(
    commit_description: Option<&str>,
    pr_description: Option<&str>,
) -> BTreeMap<String, String> {
    let mut directives = BTreeMap::new();

    if let Some(commit_description) = commit_description {
        directives.extend(parse_bypass_directives(commit_description));
    }
    if let Some(pr_description) = pr_description {
        // PR description overrides commit description when both specify the same bypass.
        directives.extend(parse_bypass_directives(pr_description));
    }

    directives
}

pub fn bypass_name_for_check_id(check_id: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_underscore = false;

    for ch in check_id.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_uppercase());
            last_was_underscore = false;
        } else if !last_was_underscore {
            normalized.push('_');
            last_was_underscore = true;
        }
    }

    while normalized.ends_with('_') {
        normalized.pop();
    }
    while normalized.starts_with('_') {
        normalized.remove(0);
    }

    format!("BYPASS_{normalized}")
}

fn bypass_value_shape(check_id: &str) -> &'static str {
    if check_id == "change/file-count" {
        "<max_files>:<reason>"
    } else {
        "<specific legitimate reason>"
    }
}

pub fn bypass_failure_guidance(check_id: &str, bypass_name: &str) -> String {
    let value_shape = bypass_value_shape(check_id);
    format!(
        "Request a one-off exception using `{bypass_name}={value_shape}` in the commit description (read in every CI context, including the merge queue) or the PR description (best-effort; not read on all CI contexts). Only for a real exception or emergency - never use bypasses for convenience."
    )
}

/// Guidance used in place of [`bypass_failure_guidance`] when `allow_bypass`
/// is set, no bypass directive resolved, *and* [`crate::input::ChangeSet::pr_description`]
/// itself is `None` — meaning checkleft could not resolve a PR description at
/// all in this environment, not merely find one without the directive.
///
/// Without this distinction, a bypass declared only in the PR description
/// reads identically to no bypass having been declared at all: the standard
/// guidance points authors at a surface ([`bypass_failure_guidance`]'s "PR
/// description") that this run structurally could not read, giving no hint
/// that the directive might already exist there.
pub fn bypass_unreachable_pr_description_guidance(check_id: &str, bypass_name: &str) -> String {
    let value_shape = bypass_value_shape(check_id);
    format!(
        "Could not resolve a PR description in this environment, so only the commit description was checked for \
         `{bypass_name}` (not found there). If the directive is declared only in the PR description, it is not \
         visible to this run — for example on a GitHub merge-queue build, or without GitHub API access. Declare \
         the bypass in the commit description instead — `{bypass_name}={value_shape}` — which is \
         read in every CI context. Only for a real exception or emergency - never use bypasses for convenience."
    )
}

/// Guidance when a matching directive is present but its latest value is empty.
///
/// An empty `BYPASS_NAME=` line is still a declaration: it wins over earlier
/// non-empty reasons for the same name and does not apply a bypass.
pub fn bypass_empty_directive_guidance(check_id: &str, bypass_name: &str) -> String {
    let value_shape = bypass_value_shape(check_id);
    format!(
        "The latest `{bypass_name}` directive is empty, so it revoked any earlier non-empty reason for this check. \
         Replace it with `{bypass_name}={value_shape}` or remove the empty line so a previous reason still applies. \
         Only for a real exception or emergency - never use bypasses for convenience."
    )
}

/// Parse `BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>`.
///
/// The numeric maximum is required; a bare reason is not treated as unlimited.
pub fn parse_change_file_count_bypass_value(raw: &str) -> Option<(usize, String)> {
    let (max_part, reason_part) = raw.split_once(':')?;
    let max_files = max_part.trim().parse().ok()?;
    let reason = reason_part.trim();
    if reason.is_empty() {
        return None;
    }
    Some((max_files, reason.to_owned()))
}

fn count_non_deleted_files(changeset: &ChangeSet) -> usize {
    changeset
        .changed_files
        .iter()
        .filter(|f| f.kind != ChangeKind::Deleted)
        .count()
}

/// Apply the bound `change/file-count` bypass to an already-failing result.
///
/// Only the host evaluates directives, after enforcing the instance bypass policy.
/// `changeset` must already be the path-scope-filtered view the guest counted.
/// Exceeded-bound remediations keep the guest's split guidance and append the
/// bound-specific follow-up; generic "how to request a bypass" lines from the
/// guest are dropped because they are superseded once a bound exists.
pub fn change_file_count_bypass_findings(
    changeset: &ChangeSet,
    raw: &str,
    bypass_name: &str,
    location: Option<Location>,
    guest_remediations: &[String],
) -> Vec<Finding> {
    let count = count_non_deleted_files(changeset);
    match parse_change_file_count_bypass_value(raw) {
        Some((approved_max, _)) if count <= approved_max => Vec::new(),
        Some((approved_max, _)) => {
            let mut remediations: Vec<String> = guest_remediations
                .iter()
                .filter(|remediation| !remediation.contains("BYPASS_"))
                .cloned()
                .collect();
            remediations.push(format!(
                "The previous exception was bound to {approved_max} files (current count {count}). Request a new approval with `{bypass_name}=<max_files>:<reason>` or split the change."
            ));
            vec![Finding {
                fixable: false,
                severity: Severity::Error,
                message: format!(
                    "changeset touches {count} non-deleted files, exceeding the approved {bypass_name} maximum of {approved_max}"
                ),
                location,
                surface: None,
                remediations,
                suggested_fix: None,
            }]
        }
        None => vec![Finding {
            fixable: false,
            severity: Severity::Error,
            message: format!(
                "{bypass_name} requires an approved maximum: `{bypass_name}=<max_files>:<reason>` (got `{raw}`). A bypass without a numeric maximum is not treated as unlimited."
            ),
            location,
            surface: None,
            remediations: vec![format!(
                "Replace the directive with `{bypass_name}=<max_files>:<reason>`, where max_files is the approved ceiling for this change."
            )],
            suggested_fix: None,
        }],
    }
}

pub fn bypass_applied_finding(bypass_name: &str, reason: &str, location: Option<Location>) -> Finding {
    Finding {
        fixable: false,
        severity: Severity::Warning,
        message: format!("check was bypassed via `{bypass_name}`"),
        location,
        surface: None,
        remediations: vec![format!(
            "Bypass reason: {reason}. Keep bypasses rare and only for legitimate exceptions."
        )],
        suggested_fix: None,
    }
}

pub fn maybe_bypass_findings(
    changeset: &ChangeSet,
    allow_bypass: bool,
    bypass_name: &str,
    trigger_files: &[PathBuf],
) -> Option<Vec<Finding>> {
    if !allow_bypass {
        return None;
    }

    let reason = changeset.bypass_reason(bypass_name).filter(|value| !value.is_empty())?;
    let location = trigger_files.first().cloned().map(|path| Location {
        path,
        line: None,
        column: None,
    });

    Some(vec![bypass_applied_finding(bypass_name, &reason, location)])
}

fn normalize_bypass_name(raw_name: &str) -> String {
    raw_name.trim().to_ascii_uppercase()
}

fn is_valid_bypass_name(name: &str) -> bool {
    name.starts_with("BYPASS_")
        && name.len() > "BYPASS_".len()
        && name
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch == '_' || ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{
        bypass_applied_finding, bypass_empty_directive_guidance, bypass_failure_guidance, bypass_name_for_check_id,
        bypass_unreachable_pr_description_guidance, change_file_count_bypass_findings, maybe_bypass_findings,
        parse_bypass_directives, parse_bypass_directives_from_descriptions, parse_change_file_count_bypass_value,
    };
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::{Location, Severity};
    use std::path::PathBuf;

    #[test]
    fn parses_bypass_directives_from_freeform_text() {
        let parsed = parse_bypass_directives(
            r#"
                some markdown text
                BYPASS_API_BREAKING_SURFACE=This does not change public API behavior.
                BYPASS_FRONTEND_NO_LEGACY_API = Legacy import remains in test fixture only.
            "#,
        );

        assert_eq!(
            parsed.get("BYPASS_API_BREAKING_SURFACE"),
            Some(&"This does not change public API behavior.".to_owned())
        );
        assert_eq!(
            parsed.get("BYPASS_FRONTEND_NO_LEGACY_API"),
            Some(&"Legacy import remains in test fixture only.".to_owned())
        );
    }

    #[test]
    fn retains_empty_values_but_ignores_invalid_names() {
        let parsed = parse_bypass_directives(
            r#"
                BYPASS_API_BREAKING_SURFACE=
                BYPASS API_BREAKING_SURFACE=Bad name
                BYPASS_MISSING_EQUALS
            "#,
        );

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed["BYPASS_API_BREAKING_SURFACE"], "");
    }

    #[test]
    fn maps_check_id_to_expected_bypass_name() {
        assert_eq!(bypass_name_for_check_id("file/ifchange"), "BYPASS_FILE_IFCHANGE");
        assert_eq!(
            bypass_name_for_check_id("frontend-no-legacy-api"),
            "BYPASS_FRONTEND_NO_LEGACY_API"
        );
    }

    #[test]
    fn maps_namespaced_check_id_to_expected_bypass_name() {
        // Slashes and hyphens both become underscores; result is uppercased.
        assert_eq!(bypass_name_for_check_id("format/rust"), "BYPASS_FORMAT_RUST");
        assert_eq!(bypass_name_for_check_id("format/bazel"), "BYPASS_FORMAT_BAZEL");
        assert_eq!(bypass_name_for_check_id("lint/rust"), "BYPASS_LINT_RUST");
        assert_eq!(bypass_name_for_check_id("lint/bazel"), "BYPASS_LINT_BAZEL");
        assert_eq!(
            bypass_name_for_check_id("rust/giant-structs"),
            "BYPASS_RUST_GIANT_STRUCTS"
        );
        assert_eq!(
            bypass_name_for_check_id("change/file-count"),
            "BYPASS_CHANGE_FILE_COUNT"
        );
    }

    #[test]
    fn pr_description_overrides_commit_description() {
        let parsed = parse_bypass_directives_from_descriptions(
            Some("BYPASS_API_BREAKING_SURFACE=From commit"),
            Some("BYPASS_API_BREAKING_SURFACE=From pr"),
        );

        assert_eq!(parsed.get("BYPASS_API_BREAKING_SURFACE"), Some(&"From pr".to_owned()));
    }

    #[test]
    fn empty_latest_directive_revokes_earlier_reason() {
        let parsed = parse_bypass_directives_from_descriptions(
            Some("BYPASS_FILE_SIZE=Generated fixture cannot be split."),
            Some("BYPASS_FILE_SIZE="),
        );

        assert_eq!(parsed.get("BYPASS_FILE_SIZE"), Some(&"".to_owned()));
    }

    #[test]
    fn bypass_failure_guidance_includes_strict_wording() {
        let guidance = bypass_failure_guidance("api/breaking-surface", "BYPASS_API_BREAKING_SURFACE");
        assert!(guidance.contains("BYPASS_API_BREAKING_SURFACE=<specific legitimate reason>"));
        assert!(guidance.contains("never use bypasses for convenience"));
    }

    #[test]
    fn bypass_failure_guidance_steers_to_commit_description_first() {
        let guidance = bypass_failure_guidance("change/file-count", "BYPASS_CHANGE_FILE_COUNT");
        let commit_pos = guidance
            .find("commit description")
            .expect("mentions commit description");
        let pr_pos = guidance.find("PR description").expect("mentions PR description");
        assert!(
            commit_pos < pr_pos,
            "commit description must be presented before PR description: {guidance}"
        );
    }

    #[test]
    fn bypass_unreachable_pr_description_guidance_explains_why_and_names_directive() {
        let guidance = bypass_unreachable_pr_description_guidance("change/file-count", "BYPASS_CHANGE_FILE_COUNT");
        assert!(guidance.contains("Could not resolve a PR description"));
        assert!(guidance.contains("BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>"));
        assert!(guidance.contains("commit description"));
        assert!(guidance.contains("never use bypasses for convenience"));
    }

    #[test]
    fn bypass_empty_directive_guidance_names_revocation_and_shape() {
        let guidance = bypass_empty_directive_guidance("file/size", "BYPASS_FILE_SIZE");
        assert!(guidance.contains("The latest `BYPASS_FILE_SIZE` directive is empty"));
        assert!(guidance.contains("revoked any earlier non-empty reason"));
        assert!(guidance.contains("BYPASS_FILE_SIZE=<specific legitimate reason>"));
        assert!(guidance.contains("never use bypasses for convenience"));
    }

    fn oversized_changeset(n: usize) -> ChangeSet {
        ChangeSet::new(
            (0..n)
                .map(|i| ChangedFile {
                    path: PathBuf::from(format!("f{i}.rs")),
                    kind: ChangeKind::Added,
                    old_path: None,
                })
                .collect(),
        )
    }

    #[test]
    fn parse_change_file_count_bypass_value_requires_max_and_reason() {
        assert_eq!(
            parse_change_file_count_bypass_value("8:Coordinated rename."),
            Some((8, "Coordinated rename.".to_owned()))
        );
        assert_eq!(
            parse_change_file_count_bypass_value(" 64 : reason with: colons "),
            Some((64, "reason with: colons".to_owned()))
        );
        assert_eq!(parse_change_file_count_bypass_value("Coordinated rename."), None);
        assert_eq!(parse_change_file_count_bypass_value("many:rename"), None);
        assert_eq!(parse_change_file_count_bypass_value("8:"), None);
        assert_eq!(parse_change_file_count_bypass_value("8:   "), None);
    }

    #[test]
    fn change_file_count_bypass_within_approved_max_passes() {
        let findings = change_file_count_bypass_findings(
            &oversized_changeset(6),
            "8:Coordinated rename of the protocol surface.",
            "BYPASS_CHANGE_FILE_COUNT",
            None,
            &[],
        );
        assert!(findings.is_empty(), "findings: {findings:?}");
    }

    #[test]
    fn change_file_count_bypass_above_approved_max_fails_with_counts() {
        let guest_remediations = vec![
            "Feature work is resolved by splitting it into smaller stacked PRs.".to_owned(),
            "request a one-off exception with `BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>`".to_owned(),
        ];
        let findings = change_file_count_bypass_findings(
            &oversized_changeset(10),
            "8:Coordinated rename of the protocol surface.",
            "BYPASS_CHANGE_FILE_COUNT",
            None,
            &guest_remediations,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(
            findings[0].message.contains("10 non-deleted files") && findings[0].message.contains('8'),
            "message was: {}",
            findings[0].message
        );
        assert_eq!(
            findings[0].remediations,
            vec![
                "Feature work is resolved by splitting it into smaller stacked PRs.".to_owned(),
                "The previous exception was bound to 8 files (current count 10). Request a new approval with `BYPASS_CHANGE_FILE_COUNT=<max_files>:<reason>` or split the change.".to_owned(),
            ]
        );
    }

    #[test]
    fn change_file_count_bypass_missing_numeric_max_is_rejected() {
        let findings = change_file_count_bypass_findings(
            &oversized_changeset(6),
            "Coordinated rename of the protocol surface.",
            "BYPASS_CHANGE_FILE_COUNT",
            None,
            &[],
        );
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("approved maximum")
                && findings[0].message.contains("not treated as unlimited"),
            "message was: {}",
            findings[0].message
        );
    }

    #[test]
    fn bypass_applied_finding_is_warning_with_reason() {
        let finding = bypass_applied_finding(
            "BYPASS_API_BREAKING_SURFACE",
            "No public API surface changed.",
            Some(Location {
                path: PathBuf::from("backend/blob/src/v3/auth.rs"),
                line: None,
                column: None,
            }),
        );

        assert_eq!(finding.severity, Severity::Warning);
        assert!(finding.message.contains("BYPASS_API_BREAKING_SURFACE"));
        assert!(
            finding
                .remediations
                .iter()
                .any(|r| r.contains("No public API surface changed."))
        );
    }

    #[test]
    fn maybe_bypass_findings_returns_warning_finding_when_enabled_and_present() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("backend/blob/src/v3/auth.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_API_BREAKING_SURFACE=No public API surface changed.".to_owned(),
        ));
        let trigger_files = vec![PathBuf::from("backend/blob/src/v3/auth.rs")];

        let findings = maybe_bypass_findings(&changeset, true, "BYPASS_API_BREAKING_SURFACE", &trigger_files)
            .expect("expected bypass findings");

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
    }

    #[test]
    fn maybe_bypass_findings_is_none_when_disabled() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("backend/blob/src/v3/auth.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_API_BREAKING_SURFACE=No public API surface changed.".to_owned(),
        ));
        let trigger_files = vec![PathBuf::from("backend/blob/src/v3/auth.rs")];

        assert!(maybe_bypass_findings(&changeset, false, "BYPASS_API_BREAKING_SURFACE", &trigger_files).is_none());
    }
}
