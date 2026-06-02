//! Buildifier check for checkleft.
//!
//! Runs `buildifier` on each changed Starlark file in the changeset and converts its
//! JSON output to checkleft findings. Requires buildifier 7+ (`--format=json` support).
//! Only files buildifier understands are inspected; unchanged files are never touched.
//!
//! Two passes are run per file: a format pass (`--mode=check`) and a lint pass
//! (`--lint=warn`). Separate invocations give cleaner exit-code semantics — format
//! issues return exit 4, lint warnings return exit 5 — and distinct JSON shapes that
//! are each easier to parse.
//!
//! # Invoking buildifier
//!
//! `buildifier_path` accepts either a single binary path OR a full `bazel run`
//! invocation. When a `bazel run` form is used, checkleft automatically inserts
//! a `--` separator so buildifier's own flags are not interpreted as Bazel flags:
//!
//!   bazel run @buildifier_prebuilt//:buildifier -- --mode=check --format=json ...
//!
//! Bazel writes build-progress output to stderr; checkleft only parses stdout so
//! bazel chatter does not corrupt the JSON.
//!
//! # Sample CHECKS.yaml entry
//!
//! ```yaml
//! - id: buildifier
//!   config:
//!     # Single binary (repobin, PATH, absolute path):
//!     buildifier_path: "bin/buildifier"
//!     # Or a bazel run invocation (works in any repo with buildifier_prebuilt):
//!     # buildifier_path: "bazel run @buildifier_prebuilt//:buildifier"
//!     # check_format: true   # set false to skip formatting pass
//!     # check_lint: true     # set false to skip lint pass
//! ```
//!
//! The default when no `buildifier_path` is configured is
//! `"bazel run @buildifier_prebuilt//:buildifier"`, which works out-of-the-box in
//! any Bazel workspace that depends on the `buildifier_prebuilt` module.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

// ── public check ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct BuildifierCheck;

#[async_trait]
impl Check for BuildifierCheck {
    fn id(&self) -> &str {
        "buildifier"
    }

    fn description(&self) -> &str {
        "runs buildifier on changed Starlark files, reporting formatting and lint violations"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

// ── config ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BuildifierConfigRaw {
    #[serde(default = "default_buildifier_invocation")]
    buildifier_path: String,
    #[serde(default = "default_true")]
    check_format: bool,
    #[serde(default = "default_true")]
    check_lint: bool,
}

fn default_buildifier_invocation() -> String {
    "bazel run @buildifier_prebuilt//:buildifier".to_owned()
}

fn default_true() -> bool {
    true
}

struct BuildifierConfig {
    /// The buildifier invocation string: either a single binary path (e.g. `"bin/buildifier"`)
    /// or a multi-token `bazel run` form (e.g. `"bazel run @buildifier_prebuilt//:buildifier"`).
    buildifier_invocation: String,
    check_format: bool,
    check_lint: bool,
}

fn parse_config(config: &toml::Value) -> Result<BuildifierConfig> {
    let raw: BuildifierConfigRaw = config
        .clone()
        .try_into()
        .context("invalid buildifier check config")?;
    Ok(BuildifierConfig {
        buildifier_invocation: raw.buildifier_path,
        check_format: raw.check_format,
        check_lint: raw.check_lint,
    })
}

/// Returns `true` when the invocation string is a `bazel run <target>` command.
///
/// Such invocations require a `--` separator before buildifier's own flags so
/// that Bazel does not consume them as its own startup/command options.
fn is_bazel_run(invocation: &str) -> bool {
    let mut tokens = invocation.split_whitespace();
    let prog = tokens.next().unwrap_or("");
    let sub = tokens.next().unwrap_or("");
    (prog == "bazel" || prog.ends_with("/bazel")) && sub == "run"
}

// ── ConfiguredCheck impl ─────────────────────────────────────────────────────

#[async_trait]
impl ConfiguredCheck for BuildifierConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        if !self.check_format && !self.check_lint {
            return Ok(CheckResult {
                check_id: "buildifier".to_owned(),
                findings: Vec::new(),
            });
        }

        let mut findings = Vec::new();

        for changed_file in &changeset.changed_files {
            if matches!(changed_file.kind, ChangeKind::Deleted) {
                continue;
            }
            if !is_buildifier_file(&changed_file.path) {
                continue;
            }

            let Ok(contents) = tree.read_file(&changed_file.path) else {
                continue;
            };

            if self.check_format {
                match run_format_check(&self.buildifier_invocation, &changed_file.path, &contents) {
                    Ok(file_findings) => findings.extend(file_findings),
                    Err(e) => findings.push(spawn_error_finding(
                        &changed_file.path,
                        &self.buildifier_invocation,
                        &e,
                    )),
                }
            }

            if self.check_lint {
                match run_lint_check(&self.buildifier_invocation, &changed_file.path, &contents) {
                    Ok(file_findings) => findings.extend(file_findings),
                    Err(e) => findings.push(spawn_error_finding(
                        &changed_file.path,
                        &self.buildifier_invocation,
                        &e,
                    )),
                }
            }
        }

        Ok(CheckResult {
            check_id: "buildifier".to_owned(),
            findings,
        })
    }
}

fn spawn_error_finding(
    file_path: &Path,
    buildifier_invocation: &str,
    e: &anyhow::Error,
) -> Finding {
    Finding {
        severity: Severity::Warning,
        message: format!("could not run buildifier on `{}`: {e}", file_path.display()),
        location: Some(Location {
            path: file_path.to_path_buf(),
            line: None,
            column: None,
        }),
        remediations: vec![format!(
            "Ensure buildifier is available via `{buildifier_invocation}`."
        )],
        suggested_fix: None,
    }
}

// ── file-kind filter ──────────────────────────────────────────────────────────

/// Returns `true` for file names / extensions that buildifier processes.
pub(crate) fn is_buildifier_file(path: &Path) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some("BUILD" | "BUILD.bazel" | "MODULE.bazel" | "WORKSPACE" | "WORKSPACE.bazel") => true,
        _ => matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("bzl" | "star")
        ),
    }
}

// ── buildifier invocations ────────────────────────────────────────────────────

/// Runs the format pass (`--mode=check --format=json`) and returns a finding if the file
/// needs reformatting.
fn run_format_check(
    buildifier_invocation: &str,
    file_path: &Path,
    contents: &[u8],
) -> Result<Vec<Finding>> {
    let path_flag = format!("-path={}", file_path.to_string_lossy());
    let output = invoke_buildifier(
        buildifier_invocation,
        &["--mode=check", "--format=json", &path_flag, "-"],
        contents,
    )?;
    parse_format_output(&output.stdout, file_path)
}

/// Runs the lint pass (`--lint=warn --format=json`) and returns one finding per warning.
fn run_lint_check(
    buildifier_invocation: &str,
    file_path: &Path,
    contents: &[u8],
) -> Result<Vec<Finding>> {
    let path_flag = format!("-path={}", file_path.to_string_lossy());
    let output = invoke_buildifier(
        buildifier_invocation,
        &["--lint=warn", "--format=json", &path_flag, "-"],
        contents,
    )?;
    parse_lint_output(&output.stdout, file_path)
}

/// Spawns the buildifier process described by `invocation` and returns its output.
///
/// `invocation` may be a single binary path (`"bin/buildifier"`) or a multi-token
/// `bazel run` command (`"bazel run @buildifier_prebuilt//:buildifier"`).
/// When a `bazel run` form is detected, `--` is automatically inserted before
/// `buildifier_args` so that Bazel does not consume them as its own flags.
///
/// Only `stdout` is used to parse buildifier's JSON; Bazel's build-progress chatter
/// goes to `stderr` and is never passed to the JSON parser.
fn invoke_buildifier(
    invocation: &str,
    buildifier_args: &[&str],
    contents: &[u8],
) -> Result<Output> {
    let mut tokens = invocation.split_whitespace();
    let program = tokens
        .next()
        .ok_or_else(|| anyhow::anyhow!("buildifier_path is empty"))?;
    let invocation_args: Vec<&str> = tokens.collect();

    let mut cmd = Command::new(program);
    cmd.args(&invocation_args);
    if is_bazel_run(invocation) {
        // Bazel requires `--` before the run target's own flags.
        cmd.arg("--");
    }
    cmd.args(buildifier_args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{invocation}`"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(contents)
            .context("failed to write to buildifier stdin")?;
    }

    child
        .wait_with_output()
        .context("failed to wait for buildifier")
}

// ── JSON output parsing ───────────────────────────────────────────────────────

/// Parses `--mode=check --format=json` output and returns a finding if the file is
/// not formatted.
pub(crate) fn parse_format_output(stdout: &[u8], file_path: &Path) -> Result<Vec<Finding>> {
    let json: BuildifierOutput =
        serde_json::from_slice(stdout).context("failed to parse buildifier format JSON output")?;

    let mut findings = Vec::new();
    for file in json.files {
        if !file.formatted.unwrap_or(true) {
            findings.push(Finding {
                severity: Severity::Warning,
                message: "file needs buildifier formatting".to_owned(),
                location: Some(Location {
                    path: file_path.to_path_buf(),
                    line: None,
                    column: None,
                }),
                remediations: vec![format!(
                    "Run `buildifier {}` to auto-format.",
                    file_path.display()
                )],
                suggested_fix: None,
            });
        }
    }
    Ok(findings)
}

/// Parses `--lint=warn --format=json` output and returns one finding per warning.
pub(crate) fn parse_lint_output(stdout: &[u8], file_path: &Path) -> Result<Vec<Finding>> {
    let json: BuildifierOutput =
        serde_json::from_slice(stdout).context("failed to parse buildifier lint JSON output")?;

    let mut findings = Vec::new();
    for file in json.files {
        for warning in file.warnings.unwrap_or_default() {
            findings.push(Finding {
                severity: Severity::Warning,
                message: format!("{}: {}", warning.category, warning.message),
                location: Some(Location {
                    path: file_path.to_path_buf(),
                    line: Some(warning.start.line),
                    column: Some(warning.start.column),
                }),
                remediations: vec![
                    "Run `buildifier --lint=fix` to auto-fix, or resolve manually.".to_owned(),
                ],
                suggested_fix: None,
            });
        }
    }
    Ok(findings)
}

// ── JSON types ────────────────────────────────────────────────────────────────

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

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{BuildifierCheck, is_bazel_run, is_buildifier_file, parse_format_output, parse_lint_output};
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::Severity;
    use crate::source_tree::LocalSourceTree;

    // ── format JSON parser tests ─────────────────────────────────────────────

    #[test]
    fn format_output_detects_unformatted_file() {
        let json = br#"{"success":false,"files":[{"filename":"foo.bzl","formatted":false}]}"#;
        let findings = parse_format_output(json, Path::new("foo.bzl")).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(findings[0].message.contains("formatting"), "unexpected: {}", findings[0].message);
        assert!(findings[0].location.as_ref().unwrap().line.is_none());
    }

    #[test]
    fn format_output_no_finding_when_formatted() {
        let json = br#"{"success":true,"files":[{"filename":"foo.bzl","formatted":true}]}"#;
        let findings = parse_format_output(json, Path::new("foo.bzl")).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn format_output_no_finding_when_formatted_absent() {
        // `formatted` absent → treated as true (already formatted)
        let json = br#"{"success":true,"files":[{"filename":"foo.bzl"}]}"#;
        let findings = parse_format_output(json, Path::new("foo.bzl")).unwrap();
        assert!(findings.is_empty());
    }

    // ── lint JSON parser tests ───────────────────────────────────────────────

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
        let findings = parse_lint_output(json, Path::new("foo.bzl")).unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.message.contains("module-docstring"), "unexpected: {}", f.message);
        let loc = f.location.as_ref().unwrap();
        assert_eq!(loc.line, Some(10));
        assert_eq!(loc.column, Some(5));
    }

    #[test]
    fn lint_output_parses_multiple_warnings() {
        let json = br#"{
            "success": false,
            "files": [{
                "filename": "BUILD",
                "warnings": [
                    {"start": {"line": 1, "column": 1}, "end": {"line": 1, "column": 1},
                     "category": "module-docstring", "actionable": true,
                     "message": "missing docstring"},
                    {"start": {"line": 5, "column": 3}, "end": {"line": 5, "column": 3},
                     "category": "no-effect", "actionable": true,
                     "message": "expression has no effect"}
                ]
            }]
        }"#;
        let findings = parse_lint_output(json, Path::new("BUILD")).unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].location.as_ref().unwrap().line, Some(1));
        assert_eq!(findings[1].location.as_ref().unwrap().line, Some(5));
    }

    #[test]
    fn lint_output_no_findings_when_warnings_absent() {
        let json = br#"{"success":true,"files":[{"filename":"foo.bzl"}]}"#;
        let findings = parse_lint_output(json, Path::new("foo.bzl")).unwrap();
        assert!(findings.is_empty());
    }

    // ── bazel run detection ──────────────────────────────────────────────────

    #[test]
    fn is_bazel_run_recognises_bazel_run_invocations() {
        assert!(is_bazel_run("bazel run @buildifier_prebuilt//:buildifier"));
        assert!(is_bazel_run("bazel run //tools:buildifier"));
        assert!(is_bazel_run("/usr/local/bin/bazel run @foo//:bar"));
    }

    #[test]
    fn is_bazel_run_rejects_single_binary_paths() {
        assert!(!is_bazel_run("buildifier"));
        assert!(!is_bazel_run("bin/buildifier"));
        assert!(!is_bazel_run("/usr/local/bin/buildifier"));
        assert!(!is_bazel_run("bazel build //..."));
    }

    // ── file-kind filter ─────────────────────────────────────────────────────

    #[test]
    fn recognises_bzl_and_star_extensions() {
        assert!(is_buildifier_file(Path::new("rules.bzl")));
        assert!(is_buildifier_file(Path::new("lib/helpers.bzl")));
        assert!(is_buildifier_file(Path::new("macros.star")));
    }

    #[test]
    fn recognises_special_filenames() {
        for name in [
            "BUILD",
            "BUILD.bazel",
            "MODULE.bazel",
            "WORKSPACE",
            "WORKSPACE.bazel",
        ] {
            assert!(
                is_buildifier_file(Path::new(name)),
                "{name} should be recognised as a Starlark file"
            );
        }
    }

    #[test]
    fn rejects_non_starlark_files() {
        for name in ["main.rs", "Cargo.toml", "README.md", "script.py", "foo.txt"] {
            assert!(
                !is_buildifier_file(Path::new(name)),
                "{name} should not be recognised as a Starlark file"
            );
        }
    }

    // ── integration: changeset scoping (no buildifier binary required) ───────

    #[tokio::test]
    async fn non_starlark_file_in_changeset_produces_no_findings() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();

        let check = BuildifierCheck;
        let tree = LocalSourceTree::new(temp.path()).unwrap();
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("main.rs").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .unwrap();

        assert!(
            result.findings.is_empty(),
            "non-Starlark files must be skipped; got: {:?}",
            result.findings
        );
    }

    #[tokio::test]
    async fn deleted_starlark_file_produces_no_findings() {
        let temp = tempdir().unwrap();

        let check = BuildifierCheck;
        let tree = LocalSourceTree::new(temp.path()).unwrap();
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("deleted.bzl").to_path_buf(),
                    kind: ChangeKind::Deleted,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .unwrap();

        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn both_checks_disabled_produces_no_findings() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("file.bzl"), "def foo(): pass\n").unwrap();

        let check = BuildifierCheck;
        let tree = LocalSourceTree::new(temp.path()).unwrap();
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("file.bzl").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    check_format = false
                    check_lint = false
                }),
            )
            .await
            .unwrap();

        assert!(result.findings.is_empty());
    }
}
