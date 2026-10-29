//! The build gate follows the repo's detected build system: Bazel repos keep
//! the Bazel gate, other repos get their own documented commands, and a repo
//! with no markers is told to report that no command was found.

use super::{
    ExecutionPromptParams, base_execution, bazel_workspace, chore_without_pr, compose_execution_prompt,
    revision_execution, revision_task_with_created_via, sample_conflict_attempt,
};

fn npm_workspace() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
    dir
}

fn conflict_prompt(ws: &std::path::Path) -> String {
    let work_item = revision_task_with_created_via(None, "merge-conflict:crz_frag_01");
    let attempt = sample_conflict_attempt();
    compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(ws)
            .conflict_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    )
}

fn chore_prompt(ws: &std::path::Path) -> String {
    compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    )
}

/// The caching rule and the generic blocker-syntax example mention Bazel in
/// every prompt; strip them so assertions look only at gate text.
fn without_cache_rule(prompt: &str) -> String {
    prompt
        .lines()
        .filter(|line| !line.starts_with("**Trust Bazel's caching.**") && !line.contains("E0583"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn bazel_conflict_revision_keeps_bazel_gate() {
    let ws = bazel_workspace();
    let prompt = conflict_prompt(ws.path());
    assert!(prompt.contains("## Pre-push gate for conflict resolution (Bazel workspace)"));
    assert!(prompt.contains("MUST run the `bazel build` merge-correctness gate"));
    assert!(prompt.contains("`bazel mod deps --lockfile_mode=update`"));
    assert!(!prompt.contains("non-Bazel repository"));
}

#[test]
fn npm_conflict_revision_gets_npm_gate_without_bazel_commands() {
    let ws = npm_workspace();
    let prompt = without_cache_rule(&conflict_prompt(ws.path()));
    assert!(prompt.contains("non-Bazel repository"), "{prompt}");
    assert!(prompt.contains("`package.json` scripts"), "{prompt}");
    assert!(prompt.contains("npm run typecheck"), "{prompt}");
    assert!(prompt.contains("`package-lock.json`"), "{prompt}");
    assert!(
        prompt.contains("MUST run the repo's own documented compile and type-check commands"),
        "{prompt}"
    );
    assert!(
        !prompt.contains("bazel build"),
        "bazel build leaked into npm prompt:\n{prompt}"
    );
    assert!(!prompt.contains("bazel mod"), "{prompt}");
    assert!(!prompt.contains("bazel test"), "{prompt}");
    assert!(!prompt.contains("bazel query"), "{prompt}");
}

#[test]
fn npm_chore_gets_npm_gate_without_bazel_commands() {
    let ws = npm_workspace();
    let prompt = without_cache_rule(&chore_prompt(ws.path()));
    assert!(
        prompt.contains("## Pre-push build gate (non-Bazel repository)"),
        "{prompt}"
    );
    assert!(!prompt.contains("bazel build"), "{prompt}");
    assert!(!prompt.contains("bazel test"), "{prompt}");
}

#[test]
fn repo_without_markers_is_told_to_report_missing_build_command() {
    let ws = tempfile::TempDir::new().unwrap();
    let prompt = without_cache_rule(&conflict_prompt(ws.path()));
    assert!(prompt.contains("No build-system manifest was detected"), "{prompt}");
    assert!(prompt.contains("no build command was found"), "{prompt}");
    assert!(prompt.contains("do not invent one"), "{prompt}");
    assert!(!prompt.contains("bazel build"), "{prompt}");
}
