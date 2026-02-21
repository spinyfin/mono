use std::path::PathBuf;

use checkleft::output::{CheckResult, FileEdit, Finding, Location, Severity, SuggestedFix};

use super::{
    OutputStyle, normalize_optional_description, render_human_results, sort_results_for_output,
};

#[test]
fn human_output_includes_line_and_column() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "typo".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "Found typo `teh`; use `the` instead.".to_owned(),
                location: Some(Location {
                    path: PathBuf::from("docs/CHECKS.toml"),
                    line: Some(12),
                    column: Some(5),
                }),
                remediation: Some("Replace typo.".to_owned()),
                suggested_fix: None,
            }],
        }],
        OutputStyle { color: false },
    );

    assert!(output.contains("error[typo]: Found typo `teh`; use `the` instead."));
    assert!(output.contains("  --> docs/CHECKS.toml:12:5"));
    assert!(output.contains("   = help: Replace typo."));
}

#[test]
fn human_output_omits_ansi_when_color_is_disabled() {
    let output = render_human_results(
        &[CheckResult {
            check_id: "file-size".to_owned(),
            findings: vec![Finding {
                severity: Severity::Warning,
                message: "File exceeds configured line count.".to_owned(),
                location: Some(Location {
                    path: PathBuf::from("backend/src/lib.rs"),
                    line: Some(200),
                    column: None,
                }),
                remediation: None,
                suggested_fix: Some(SuggestedFix {
                    description: "Split file by module.".to_owned(),
                    edits: vec![FileEdit {
                        path: PathBuf::from("backend/src/lib.rs"),
                        old_text: "old".to_owned(),
                        new_text: "new".to_owned(),
                    }],
                }),
            }],
        }],
        OutputStyle { color: false },
    );

    assert!(!output.contains("\u{1b}["));
    assert!(output.contains("  --> backend/src/lib.rs:200"));
    assert!(output.contains("   = fix: Split file by module. (1 edit)"));
}

#[test]
fn output_sorting_prioritizes_error_checks_before_warning_checks() {
    let mut results = vec![
        CheckResult {
            check_id: "alpha-warning".to_owned(),
            findings: vec![Finding {
                severity: Severity::Warning,
                message: "warning finding".to_owned(),
                location: None,
                remediation: None,
                suggested_fix: None,
            }],
        },
        CheckResult {
            check_id: "zeta-error".to_owned(),
            findings: vec![Finding {
                severity: Severity::Error,
                message: "error finding".to_owned(),
                location: None,
                remediation: None,
                suggested_fix: None,
            }],
        },
    ];

    sort_results_for_output(&mut results);

    assert_eq!(results[0].check_id, "zeta-error");
    assert_eq!(results[1].check_id, "alpha-warning");
}

#[test]
fn output_sorting_orders_findings_within_each_check_by_severity() {
    let mut results = vec![CheckResult {
        check_id: "mixed".to_owned(),
        findings: vec![
            Finding {
                severity: Severity::Warning,
                message: "warning finding".to_owned(),
                location: None,
                remediation: None,
                suggested_fix: None,
            },
            Finding {
                severity: Severity::Info,
                message: "info finding".to_owned(),
                location: None,
                remediation: None,
                suggested_fix: None,
            },
            Finding {
                severity: Severity::Error,
                message: "error finding".to_owned(),
                location: None,
                remediation: None,
                suggested_fix: None,
            },
        ],
    }];

    sort_results_for_output(&mut results);

    let severities: Vec<_> = results[0]
        .findings
        .iter()
        .map(|finding| finding.severity)
        .collect();
    assert_eq!(
        severities,
        vec![Severity::Error, Severity::Warning, Severity::Info]
    );
}

#[test]
fn normalize_optional_description_trims_and_filters_empty_values() {
    assert_eq!(normalize_optional_description(None), None);
    assert_eq!(normalize_optional_description(Some("".to_owned())), None);
    assert_eq!(
        normalize_optional_description(Some("  235  ".to_owned())),
        Some("235".to_owned())
    );
}
