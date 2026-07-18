//! Format/lint transform-level parity tests: real buildifier `--format=json`
//! output fed through the declarative `json` transform produces the expected
//! `Vec<Finding>` (unformatted detection, clean-file no-finding, one finding per
//! warning, clean lint). Plus the exit-semantics guard that a crash exit is
//! classified as an error, never a silent clean.

use std::path::Path;

use crate::output::Severity;

use super::tests_common::{
    REAL_FORMAT_CLEAN, REAL_FORMAT_UNFORMATTED, REAL_LINT_CLEAN, REAL_LINT_WARNINGS, declarative_format_findings,
    declarative_lint_findings, parse_package,
};
use super::{ExitOutcome, ExitSemantics};

#[test]
fn format_transform_detects_unformatted_file() {
    let findings = declarative_format_findings(REAL_FORMAT_UNFORMATTED);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(
        findings[0].message.contains("formatting"),
        "expected a formatting message, got: {}",
        findings[0].message
    );
    // Format findings carry no line number (file-level, not line-level).
    assert_eq!(findings[0].location.as_ref().unwrap().line, None);
}

#[test]
fn format_transform_no_finding_on_clean_file() {
    let findings = declarative_format_findings(REAL_FORMAT_CLEAN);
    assert!(findings.is_empty());
}

#[test]
fn lint_transform_produces_one_finding_per_warning() {
    let findings = declarative_lint_findings(REAL_LINT_WARNINGS, "a/b/malformed.bzl");
    assert_eq!(findings.len(), 3);
    // Path comes from invocation context (warnings carry no filename in the JSON).
    assert_eq!(
        findings[0].location.as_ref().unwrap().path,
        Path::new("a/b/malformed.bzl")
    );
    assert_eq!(findings[0].location.as_ref().unwrap().line, Some(11));
    assert_eq!(findings[0].location.as_ref().unwrap().column, Some(1));
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(
        findings[0].message.contains("module-docstring"),
        "first warning should be module-docstring, got: {}",
        findings[0].message
    );
}

#[test]
fn lint_transform_no_findings_on_clean_file() {
    let findings = declarative_lint_findings(REAL_LINT_CLEAN, "a/b/clean.bzl");
    assert!(findings.is_empty());
}

// ── exit semantics: a crash must surface as an error, never silent-clean ────────

#[test]
fn exit_default_error_surfaces_as_transform_error() {
    // Simulate buildifier crashing (nonzero exit, non-JSON stderr-style stdout).
    // The executor's classify maps default -> Error, which aborts with an error;
    // here we assert the model classifies a crash exit as Error rather than Ok.
    let exit = exit_semantics_for_test();
    assert_eq!(exit.classify(Some(2)), ExitOutcome::Error);
    assert_eq!(exit.classify(Some(0)), ExitOutcome::Findings);
}

fn exit_semantics_for_test() -> ExitSemantics {
    let package = parse_package();
    package.invocations[0].exit.clone()
}
