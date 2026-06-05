//! buildifier JSON → checkleft `Finding` transform.
//!
//! PROTOTYPE NOTE: this is a faithful **copy** of the parsing/transform logic and
//! finding shapes from `tools/checkleft/src/checks/buildifier.rs`
//! (`parse_format_output`, `parse_lint_output`, and the `BuildifierOutput` JSON
//! types). It is copied rather than shared because this guest compiles to
//! `wasm32-unknown-unknown` and cannot depend on the `checkleft` crate (which
//! pulls in wasmtime/tokio). A production version would factor these types and
//! the parser into a small, dependency-light "protocol" crate that BOTH the
//! built-in check and the wasm guest depend on, so parity is guaranteed by
//! construction instead of by copy. See PROTOTYPE-NOTES.md.
//!
//! The `Finding` / `Location` / `Severity` types below serialize to exactly the
//! JSON that `checkleft::output::Finding` deserializes, so the host runtime reads
//! them back unchanged.

use serde::{Deserialize, Serialize};

// ── finding shapes (serialize-compatible with checkleft::output) ──────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[allow(dead_code)]
    Error,
    Warning,
    #[allow(dead_code)]
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Location {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    pub location: Option<Location>,
    #[serde(default)]
    pub remediations: Vec<String>,
    /// Always `None` for buildifier findings; serializes to `null` to match the
    /// built-in check.
    pub suggested_fix: Option<()>,
}

// ── buildifier --format=json types (copied verbatim from buildifier.rs) ───────

#[derive(Debug, Deserialize)]
struct BuildifierOutput {
    #[serde(default)]
    files: Vec<BuildifierFile>,
}

#[derive(Debug, Deserialize)]
struct BuildifierFile {
    formatted: Option<bool>,
    #[serde(default)]
    warnings: Option<Vec<BuildifierWarning>>,
}

#[derive(Debug, Deserialize)]
struct BuildifierWarning {
    start: BuildifierPosition,
    category: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct BuildifierPosition {
    line: u32,
    column: u32,
}

// ── transforms (copied from buildifier.rs, adapted to String paths) ───────────

/// Parses `--mode=check --format=json` output; one finding if the file is not
/// formatted. Mirrors `buildifier::parse_format_output`.
pub fn parse_format_output(stdout: &[u8], file_path: &str) -> Result<Vec<Finding>, String> {
    let json: BuildifierOutput = serde_json::from_slice(stdout).map_err(|err| {
        format!(
            "failed to parse buildifier format JSON output: {err}; raw stdout: {:?}",
            String::from_utf8_lossy(stdout)
        )
    })?;

    let mut findings = Vec::new();
    for file in json.files {
        if !file.formatted.unwrap_or(true) {
            findings.push(Finding {
                severity: Severity::Warning,
                message: "file needs buildifier formatting".to_owned(),
                location: Some(Location {
                    path: file_path.to_owned(),
                    line: None,
                    column: None,
                }),
                remediations: vec![format!("Run `buildifier {file_path}` to auto-format.")],
                suggested_fix: None,
            });
        }
    }
    Ok(findings)
}

/// Parses `--mode=check --lint=warn --format=json` output; one finding per
/// warning. Mirrors `buildifier::parse_lint_output`.
pub fn parse_lint_output(stdout: &[u8], file_path: &str) -> Result<Vec<Finding>, String> {
    let json: BuildifierOutput = serde_json::from_slice(stdout).map_err(|err| {
        format!(
            "failed to parse buildifier lint JSON output: {err}; raw stdout: {:?}",
            String::from_utf8_lossy(stdout)
        )
    })?;

    let mut findings = Vec::new();
    for file in json.files {
        for warning in file.warnings.unwrap_or_default() {
            findings.push(Finding {
                severity: Severity::Warning,
                message: format!("{}: {}", warning.category, warning.message),
                location: Some(Location {
                    path: file_path.to_owned(),
                    line: Some(warning.start.line),
                    column: Some(warning.start.column),
                }),
                remediations: vec![format!(
                    "Run `buildifier --lint=fix {file_path}` to auto-fix, or resolve manually."
                )],
                suggested_fix: None,
            });
        }
    }
    Ok(findings)
}

/// Returns `true` for file names / extensions buildifier processes. Mirrors
/// `buildifier::is_buildifier_file`.
pub fn is_buildifier_file(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    if matches!(
        file_name,
        "BUILD" | "BUILD.bazel" | "MODULE.bazel" | "WORKSPACE" | "WORKSPACE.bazel"
    ) {
        return true;
    }
    let ext = file_name.rsplit('.').next().unwrap_or("");
    matches!(ext, "bzl" | "star") && file_name.contains('.')
}

// ── parity tests (host target; `cargo test`) ──────────────────────────────────
//
// These assert the SAME golden inputs/outputs as the built-in check's own parser
// tests in `tools/checkleft/src/checks/buildifier.rs`. Same golden JSON in, same
// findings out → transform parity with the built-in.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_output_detects_unformatted_file() {
        let json = br#"{"success":false,"files":[{"filename":"foo.bzl","formatted":false}]}"#;
        let findings = parse_format_output(json, "foo.bzl").unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(findings[0].message.contains("formatting"));
        assert!(findings[0].location.as_ref().unwrap().line.is_none());
    }

    #[test]
    fn format_output_no_finding_when_formatted() {
        let json = br#"{"success":true,"files":[{"filename":"foo.bzl","formatted":true}]}"#;
        assert!(parse_format_output(json, "foo.bzl").unwrap().is_empty());
    }

    #[test]
    fn lint_output_parses_single_warning() {
        let json = br#"{
            "success": false,
            "files": [{
                "filename": "foo.bzl",
                "warnings": [{
                    "filename": "foo.bzl",
                    "start": {"line": 10, "column": 5},
                    "end": {"line": 10, "column": 5},
                    "category": "module-docstring",
                    "actionable": true,
                    "message": "The file has no module docstring."
                }]
            }]
        }"#;
        let findings = parse_lint_output(json, "foo.bzl").unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.message.contains("module-docstring"));
        let loc = f.location.as_ref().unwrap();
        assert_eq!(loc.line, Some(10));
        assert_eq!(loc.column, Some(5));
    }

    #[test]
    fn lint_output_combined_mode_check_shape_with_warning() {
        let json = br#"{"success":false,"files":[{"filename":"lib/rust/broker-robinhood/BUILD.bazel","formatted":false,"valid":true,"warnings":[{"start":{"line":2,"column":37},"end":{"line":2,"column":48},"category":"load","actionable":true,"autoFixable":true,"message":"Loaded symbol \"rust_binary\" is unused. Please remove it.","url":"x"}]}]}"#;
        let findings = parse_lint_output(json, "lib/rust/broker-robinhood/BUILD.bazel").unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("load"));
        assert_eq!(findings[0].location.as_ref().unwrap().line, Some(2));
        assert_eq!(findings[0].location.as_ref().unwrap().column, Some(37));
    }

    #[test]
    fn finding_serializes_to_checkleft_shape() {
        let finding = Finding {
            severity: Severity::Warning,
            message: "file needs buildifier formatting".to_owned(),
            location: Some(Location {
                path: "foo.bzl".to_owned(),
                line: None,
                column: None,
            }),
            remediations: vec!["Run `buildifier foo.bzl` to auto-format.".to_owned()],
            suggested_fix: None,
        };
        let value: serde_json::Value = serde_json::to_value(&finding).unwrap();
        assert_eq!(value["severity"], "warning");
        assert_eq!(value["location"]["path"], "foo.bzl");
        assert!(value["location"]["line"].is_null());
        assert!(value["suggested_fix"].is_null());
        assert_eq!(value["remediations"][0], "Run `buildifier foo.bzl` to auto-format.");
    }

    #[test]
    fn recognises_buildifier_files() {
        assert!(is_buildifier_file("rules.bzl"));
        assert!(is_buildifier_file("lib/helpers.bzl"));
        assert!(is_buildifier_file("BUILD.bazel"));
        assert!(is_buildifier_file("MODULE.bazel"));
        assert!(!is_buildifier_file("main.rs"));
        assert!(!is_buildifier_file("README.md"));
    }
}
