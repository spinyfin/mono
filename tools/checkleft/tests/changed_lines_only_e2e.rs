//! End-to-end test for `policy.changed_lines_only` (Phase 1 of
//! `docs/investigations/checkleft-line-scoped-detection-and-fixes.md`).
//!
//! Drives a **real** git repository through `Vcs::changeset_since` (the same
//! diff plumbing `main.rs` uses) to get a real, git-derived `ChangeSet` —
//! including its `file_diffs`/`added_line_ranges` — and feeds it through the
//! full `Runner`. A synthetic check reports one finding on a line the PR
//! actually changed and one finding on a pre-existing, untouched line of the
//! same file; the test asserts both directions of the filter with that one
//! real changeset: the changed-line finding survives, the untouched-line
//! finding does not.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tempfile::tempdir;

use checkleft::check::{Check, CheckRegistry, ConfiguredCheck};
use checkleft::config::ConfigResolver;
use checkleft::input::{ChangeSet, SourceTree};
use checkleft::output::{CheckResult, Finding, Location, Severity};
use checkleft::runner::Runner;
use checkleft::source_tree::LocalSourceTree;
use checkleft::vcs::Vcs;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A check that reports one finding on `changed_line` (expected to survive
/// `changed_lines_only`) and one on `untouched_line` (expected to be dropped),
/// both anchored to the single changed file in the changeset it is handed.
#[derive(Clone)]
struct TwoLineFindingCheck {
    changed_line: u32,
    untouched_line: u32,
}

#[async_trait]
impl Check for TwoLineFindingCheck {
    fn id(&self) -> &str {
        "two-line-finding"
    }

    fn description(&self) -> &str {
        "emits a finding on a changed line and one on an untouched line"
    }

    fn configure(&self, _config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(self.clone()))
    }
}

#[async_trait]
impl ConfiguredCheck for TwoLineFindingCheck {
    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        _tree: &dyn SourceTree,
        _on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult> {
        let path = changeset
            .changed_files
            .first()
            .expect("changeset has a changed file")
            .path
            .clone();

        let finding = |line: u32, label: &str| Finding {
            fixable: false,
            severity: Severity::Error,
            message: format!("finding on {label} line"),
            location: Some(Location {
                path: path.clone(),
                line: Some(line),
                column: Some(1),
            }),
            remediations: Vec::new(),
            suggested_fix: None,
        };

        Ok(CheckResult {
            check_id: self.id().to_owned(),
            findings: vec![
                finding(self.changed_line, "changed"),
                finding(self.untouched_line, "untouched"),
            ],
        })
    }
}

/// Build a real git repo with a 10-line file, then modify exactly one
/// interior line and commit, returning (repo root, changeset since base,
/// path of the modified file, 1-based line number that was modified).
fn build_repo_with_one_changed_line() -> (tempfile::TempDir, ChangeSet, String, u32) {
    let temp = tempdir().expect("create temp dir");
    let root = temp.path();

    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    let file_path = "src/lib.rs";
    fs::create_dir_all(root.join("src")).expect("create src dir");
    let base_lines: Vec<String> = (1..=10).map(|n| format!("line{n}")).collect();
    fs::write(root.join(file_path), base_lines.join("\n") + "\n").expect("write base file");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "base"]);
    let base_sha = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_owned();

    // Modify line 5 only; every other line is untouched.
    let changed_line: u32 = 5;
    let mut lines = base_lines;
    lines[(changed_line - 1) as usize] = "line5-modified".to_owned();
    fs::write(root.join(file_path), lines.join("\n") + "\n").expect("write modified file");
    git(root, &["commit", "-aq", "-m", "modify line 5"]);

    let vcs = Vcs::detect(root).expect("detect vcs");
    let changeset = vcs.changeset_since(&base_sha).expect("changeset since base");

    (temp, changeset, file_path.to_owned(), changed_line)
}

#[tokio::test]
async fn changed_lines_only_keeps_changed_line_and_drops_untouched_line() {
    let (temp, changeset, file_path, changed_line) = build_repo_with_one_changed_line();
    let root = temp.path();

    // Sanity-check the real, git-derived diff data before trusting the runner
    // to filter with it: the parser must have recorded exactly the modified
    // line as an added-line range for this file.
    let ranges = changeset
        .changed_lines(Path::new(&file_path))
        .expect("file has diff data");
    assert!(
        ranges
            .iter()
            .any(|&(start, end)| changed_line >= start && changed_line <= end),
        "expected changed line {changed_line} to be within added ranges {ranges:?}"
    );
    let untouched_line: u32 = 1;
    assert!(
        !ranges
            .iter()
            .any(|&(start, end)| untouched_line >= start && untouched_line <= end),
        "expected untouched line {untouched_line} to fall outside added ranges {ranges:?}"
    );

    fs::write(
        root.join("CHECKS.toml"),
        r#"
[[checks]]
id = "two-line-finding"
check = "two-line-finding"

[checks.policy]
changed_lines_only = true
"#,
    )
    .expect("write CHECKS.toml");

    let mut registry = CheckRegistry::new();
    registry
        .register(TwoLineFindingCheck {
            changed_line,
            untouched_line,
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(root).expect("resolver")),
        Arc::new(LocalSourceTree::new(root).expect("tree")),
    );

    let results = runner.run_changeset(&changeset).await.expect("run checks");

    assert_eq!(results.len(), 1);
    let lines: Vec<u32> = results[0]
        .findings
        .iter()
        .map(|f| f.location.as_ref().and_then(|l| l.line).expect("finding has a line"))
        .collect();
    assert_eq!(
        lines,
        vec![changed_line],
        "expected only the changed-line finding to survive changed_lines_only"
    );
}

#[tokio::test]
async fn changed_lines_only_defaults_off_and_keeps_both_findings() {
    let (temp, changeset, _file_path, changed_line) = build_repo_with_one_changed_line();
    let root = temp.path();
    let untouched_line: u32 = 1;

    // No `policy.changed_lines_only` set: existing file-level-only scoping
    // must be unchanged, so both findings on the one changed file survive.
    fs::write(
        root.join("CHECKS.toml"),
        r#"
[[checks]]
id = "two-line-finding"
check = "two-line-finding"
"#,
    )
    .expect("write CHECKS.toml");

    let mut registry = CheckRegistry::new();
    registry
        .register(TwoLineFindingCheck {
            changed_line,
            untouched_line,
        })
        .expect("register check");

    let runner = Runner::new(
        Arc::new(registry),
        Arc::new(ConfigResolver::new(root).expect("resolver")),
        Arc::new(LocalSourceTree::new(root).expect("tree")),
    );

    let results = runner.run_changeset(&changeset).await.expect("run checks");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].findings.len(), 2, "default behavior must be unaffected");
}
