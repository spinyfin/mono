//! Invocation orchestration for declarative checks.
//!
//! Pipeline: select matched files → resolve declared binaries → run each
//! invocation (batch or per-file) → apply exit semantics → project stdout into
//! findings → concatenate. The framework owns every step; the check is data.
//!
//! Exit semantics are load-bearing. A nonzero/`default → error` outcome aborts the
//! whole check with an error (surfaced by the runner as a check error), so a tool
//! that crashes never masquerades as "clean". `findings` runs the transform (which
//! naturally yields zero findings for clean output); `ok` short-circuits to none.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};

use crate::input::{ChangeKind, ChangeSet};
use crate::output::{CheckResult, Finding};

use super::{ExitOutcome, ExternalCheckDeclarativePackage, Invocation, InvocationMode, resolve};

/// Run a declarative check end-to-end. `repo_root` is the working directory
/// invocations run from (and the Bazel workspace, when the `bazel` resolver is used).
pub fn run_declarative_check(
    repo_root: &Path,
    package_id: &str,
    package: &ExternalCheckDeclarativePackage,
    changeset: &ChangeSet,
    config: &toml::Value,
) -> Result<CheckResult> {
    let files = select_files(changeset, &package.applies_to)?;
    if files.is_empty() {
        return Ok(CheckResult {
            check_id: package_id.to_owned(),
            findings: Vec::new(),
        });
    }

    let binaries = resolve::resolve_all(repo_root, &package.needs, config)?;

    let mut findings = Vec::new();
    for invocation in &package.invocations {
        findings.extend(run_invocation(repo_root, &binaries, invocation, &files)?);
    }

    // Deduplicate: some tools (e.g. rustfmt with per-file + module-tree recursion)
    // report the same file via multiple code paths. Keep first occurrence of each
    // (check_id, path, line, column, message, severity) tuple.
    findings = dedup_findings(findings);

    Ok(CheckResult {
        check_id: package_id.to_owned(),
        findings,
    })
}

/// Remove findings that describe the same location + issue as an earlier finding.
/// Key is `(path, line, column, message, severity)` — remediations are intentionally
/// excluded: two invocations may reach the same file via different code paths (e.g.
/// rustfmt recursing from a module root and directly from the child file), producing
/// the same issue but with slightly different remediation text.
fn dedup_findings(findings: Vec<Finding>) -> Vec<Finding> {
    use std::collections::HashSet;
    type Key = (Option<PathBuf>, Option<u32>, Option<u32>, String, u8);
    let mut seen: HashSet<Key> = HashSet::new();
    let mut out: Vec<Finding> = Vec::with_capacity(findings.len());
    for f in findings {
        let key: Key = (
            f.location.as_ref().map(|l| l.path.clone()),
            f.location.as_ref().and_then(|l| l.line),
            f.location.as_ref().and_then(|l| l.column),
            f.message.clone(),
            f.severity as u8,
        );
        if seen.insert(key) {
            out.push(f);
        }
    }
    out
}

/// Strip `repo_root` from any absolute finding paths so every finding emitted by
/// the declarative runtime is repo-relative. This is a framework-level guarantee:
/// tools may receive absolute paths (e.g. when the hermetic Bazel toolchain wrapper
/// canonicalizes inputs) and echo them back — the framework normalises before the
/// finding reaches the runner.
fn normalize_finding_paths(findings: &mut Vec<Finding>, repo_root: &Path) {
    for finding in findings.iter_mut() {
        if let Some(location) = &mut finding.location {
            if location.path.is_absolute() {
                if let Ok(relative) = location.path.strip_prefix(repo_root) {
                    location.path = relative.to_path_buf();
                }
            }
        }
    }
}

/// Select non-deleted changed files matching any `applies_to` glob, sorted for
/// determinism.
fn select_files(changeset: &ChangeSet, applies_to: &[String]) -> Result<Vec<String>> {
    let mut builder = GlobSetBuilder::new();
    for pattern in applies_to {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid applies_to glob `{pattern}`"))?);
    }
    let globset = builder.build().context("failed to build applies_to glob set")?;

    let mut files: Vec<String> = changeset
        .changed_files
        .iter()
        .filter(|file| !matches!(file.kind, ChangeKind::Deleted))
        .filter(|file| globset.is_match(&file.path))
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

fn run_invocation(
    repo_root: &Path,
    binaries: &BTreeMap<String, PathBuf>,
    invocation: &Invocation,
    files: &[String],
) -> Result<Vec<Finding>> {
    let binary = binaries.get(&invocation.run).ok_or_else(|| {
        anyhow::anyhow!(
            "invocation `{}` binary `{}` was not resolved",
            invocation.id,
            invocation.run
        )
    })?;

    let mut findings = match invocation.mode {
        InvocationMode::Batch => {
            let args = expand_batch_args(repo_root, &invocation.args, files);
            let output = spawn(repo_root, binary, &args, &invocation.id)?;
            classify_and_project(invocation, &output, None)?
        }
        InvocationMode::PerFile => {
            let mut findings = Vec::new();
            for file in files {
                let args = expand_per_file_args(repo_root, &invocation.args, file);
                let output = spawn(repo_root, binary, &args, &invocation.id)?;
                findings.extend(classify_and_project(invocation, &output, Some(file))?);
            }
            findings
        }
    };

    // Normalize absolute paths to repo-relative. Tools invoked via the hermetic
    // Bazel toolchain wrapper may receive absolute input paths and echo them back;
    // the framework strips the repo root prefix before the finding reaches the runner.
    normalize_finding_paths(&mut findings, repo_root);
    Ok(findings)
}

/// In batch mode the standalone `{{files}}` arg expands to N file args.
/// `{{repo_root}}` anywhere in an arg is substituted with the absolute repo root path.
fn expand_batch_args(repo_root: &Path, args: &[String], files: &[String]) -> Vec<String> {
    let repo_root_str = repo_root.to_string_lossy();
    let mut expanded = Vec::with_capacity(args.len() + files.len());
    for arg in args {
        if arg == "{{files}}" {
            expanded.extend(files.iter().cloned());
        } else {
            expanded.push(arg.replace("{{repo_root}}", &*repo_root_str));
        }
    }
    expanded
}

/// In per-file mode `{{file}}` is substituted in place within each arg.
/// `{{repo_root}}` anywhere in an arg is substituted with the absolute repo root path.
fn expand_per_file_args(repo_root: &Path, args: &[String], file: &str) -> Vec<String> {
    let repo_root_str = repo_root.to_string_lossy();
    args.iter()
        .map(|arg| arg.replace("{{file}}", file).replace("{{repo_root}}", &*repo_root_str))
        .collect()
}

struct InvocationOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
}

fn spawn(repo_root: &Path, binary: &Path, args: &[String], invocation_id: &str) -> Result<InvocationOutput> {
    let output = Command::new(binary)
        .args(args)
        .current_dir(repo_root)
        .output()
        .with_context(|| {
            format!(
                "failed to spawn declarative invocation `{invocation_id}` binary `{}`",
                binary.display()
            )
        })?;
    Ok(InvocationOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code(),
    })
}

fn classify_and_project(
    invocation: &Invocation,
    output: &InvocationOutput,
    input_file: Option<&str>,
) -> Result<Vec<Finding>> {
    match invocation.exit.classify(output.exit_code) {
        ExitOutcome::Ok => Ok(Vec::new()),
        ExitOutcome::Findings => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            invocation
                .transform
                .apply(&output.stdout, output.exit_code, input_file)
                .with_context(|| {
                    let file_note = input_file.map(|f| format!(" (file: {f})")).unwrap_or_default();
                    let stderr_trimmed = stderr.trim();
                    if stderr_trimmed.is_empty() {
                        format!("transform for invocation `{}`{file_note} failed", invocation.id)
                    } else {
                        format!(
                            "transform for invocation `{}`{file_note} failed; tool stderr:\n{stderr_trimmed}",
                            invocation.id
                        )
                    }
                })
        }
        ExitOutcome::Error => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "declarative invocation `{}` exited with status {} (treated as error by exit semantics): {}",
                invocation.id,
                output
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_owned()),
                stderr.trim()
            )
        }
    }
}
