/// Spike: buildifier check for checkleft.
///
/// Runs `buildifier` on each changed Starlark file in the changeset and converts its
/// output to checkleft findings.  Only files buildifier understands are inspected —
/// others pass through silently, satisfying the "does NOT scan unchanged files"
/// requirement from the spike brief.
///
/// # Sample CHECKS.toml entry
///
/// ```toml
/// [[checks]]
/// id = "buildifier"
/// check = "buildifier"
///
/// [checks.config]
/// # Path to the buildifier binary; defaults to "buildifier" found on PATH.
/// # buildifier_path = "buildifier"
///
/// # Set either to false to disable that sub-check:
/// # check_format = true
/// # check_lint   = true
/// ```
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, SourceTree};
use crate::output::{CheckResult, Finding, Location, Severity};

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
    #[serde(default = "default_buildifier_path")]
    buildifier_path: String,
    #[serde(default = "default_true")]
    check_format: bool,
    #[serde(default = "default_true")]
    check_lint: bool,
}

fn default_buildifier_path() -> String {
    "buildifier".to_owned()
}

fn default_true() -> bool {
    true
}

struct BuildifierConfig {
    buildifier_path: String,
    check_format: bool,
    check_lint: bool,
}

fn parse_config(config: &toml::Value) -> Result<BuildifierConfig> {
    let raw: BuildifierConfigRaw = config
        .clone()
        .try_into()
        .context("invalid buildifier check config")?;
    Ok(BuildifierConfig {
        buildifier_path: raw.buildifier_path,
        check_format: raw.check_format,
        check_lint: raw.check_lint,
    })
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

            match run_buildifier_on_file(
                &self.buildifier_path,
                &changed_file.path,
                &contents,
                self.check_format,
                self.check_lint,
            ) {
                Ok(file_findings) => findings.extend(file_findings),
                Err(e) => {
                    // Buildifier not found or failed to spawn — surface as a warning rather
                    // than aborting the whole check run.
                    findings.push(Finding {
                        severity: Severity::Warning,
                        message: format!(
                            "could not run buildifier on `{}`: {e}",
                            changed_file.path.display()
                        ),
                        location: Some(Location {
                            path: changed_file.path.clone(),
                            line: None,
                            column: None,
                        }),
                        remediations: vec![format!(
                            "Ensure buildifier is installed and reachable as `{}`.",
                            self.buildifier_path
                        )],
                        suggested_fix: None,
                    });
                }
            }
        }

        Ok(CheckResult {
            check_id: "buildifier".to_owned(),
            findings,
        })
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

// ── buildifier invocation ─────────────────────────────────────────────────────

/// Feeds `contents` to buildifier via stdin (using `-path` so error messages reference
/// the original repo-relative `file_path`) and returns findings.
fn run_buildifier_on_file(
    buildifier_path: &str,
    file_path: &Path,
    contents: &[u8],
    check_format: bool,
    check_lint: bool,
) -> Result<Vec<Finding>> {
    // -path tells buildifier the "display name" of the stdin content so its warnings
    // reference the original path rather than "<stdin>".
    let path_flag = format!("-path={}", file_path.to_string_lossy());

    let mut args: Vec<&str> = Vec::new();
    if check_format {
        // --mode=check: exit 4 when the file would need reformatting; no stdout output.
        args.push("--mode=check");
    }
    if check_lint {
        // --lint=warn: emit lint warnings to stderr; exit 5 when warnings are present.
        args.push("--lint=warn");
    }
    args.push(&path_flag);
    args.push("-"); // read from stdin

    let mut child = Command::new(buildifier_path)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn `{buildifier_path}`"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(contents)
            .context("failed to write to buildifier stdin")?;
    }

    let output = child
        .wait_with_output()
        .context("failed to wait for buildifier")?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut findings = Vec::new();

    // Lint warnings arrive on stderr.
    if check_lint {
        findings.extend(parse_buildifier_warnings(&stderr, file_path));
    }

    // Exit 4 means --mode=check detected reformatting is required.
    // Exit 5 means lint warnings are present (covered by the stderr parse above).
    if check_format && exit_code == 4 {
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

    Ok(findings)
}

// ── output parser ─────────────────────────────────────────────────────────────

/// Parses buildifier lint warning lines from stderr into [`Finding`]s.
///
/// Buildifier emits one line per warning:
/// ```text
/// path/to/file.bzl:LINE:COL: CATEGORY: message text
/// ```
pub(crate) fn parse_buildifier_warnings(stderr: &str, file_path: &Path) -> Vec<Finding> {
    // Matches: <anything>:<digits>:<digits>: <word-or-hyphenated-category>: <rest>
    // The path portion is intentionally loose (.+) to handle paths with colons.
    let re = match Regex::new(r"^.+:(\d+):(\d+): (\w[\w-]*: .+)$") {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    stderr
        .lines()
        .filter_map(|line| {
            let caps = re.captures(line)?;
            let lineno: u32 = caps[1].parse().ok()?;
            let colno: u32 = caps[2].parse().ok()?;
            let message = caps[3].to_owned();
            Some(Finding {
                severity: Severity::Warning,
                message,
                location: Some(Location {
                    path: file_path.to_path_buf(),
                    line: Some(lineno),
                    column: Some(colno),
                }),
                remediations: vec![
                    "Run `buildifier --lint=fix` to auto-fix, or resolve manually.".to_owned(),
                ],
                suggested_fix: None,
            })
        })
        .collect()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{BuildifierCheck, is_buildifier_file, parse_buildifier_warnings};
    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::Severity;
    use crate::source_tree::LocalSourceTree;

    // ── parser unit tests (no buildifier binary required) ────────────────────

    #[test]
    fn parses_single_warning_line() {
        let findings = parse_buildifier_warnings(
            "some/file.bzl:10:5: warning: module-docstring not found\n",
            Path::new("some/file.bzl"),
        );
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(
            f.message.contains("module-docstring"),
            "unexpected message: {}",
            f.message
        );
        let loc = f.location.as_ref().unwrap();
        assert_eq!(loc.line, Some(10));
        assert_eq!(loc.column, Some(5));
        assert_eq!(loc.path, Path::new("some/file.bzl"));
    }

    #[test]
    fn parses_multiple_warning_lines() {
        let output = concat!(
            "BUILD:1:1: warning: module-docstring not found\n",
            "BUILD:5:3: warning: function-docstring missing\n",
        );
        let findings = parse_buildifier_warnings(output, Path::new("BUILD"));
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].location.as_ref().unwrap().line, Some(1));
        assert_eq!(findings[1].location.as_ref().unwrap().line, Some(5));
    }

    #[test]
    fn ignores_non_matching_lines() {
        let output = "\nnot a warning\nbuildifier: could not parse\n";
        let findings = parse_buildifier_warnings(output, Path::new("file.bzl"));
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
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
