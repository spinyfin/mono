//! The build gate follows the repo's detected build system: Bazel repos keep
//! the Bazel gate, other repos get their own documented commands, and a repo
//! with no markers is told to report that no command was found.

use super::{
    ExecutionPromptParams, base_execution, bazel_workspace, chore_without_pr, compose_execution_prompt,
    revision_execution, revision_task_with_created_via, sample_ci_attempt, sample_conflict_attempt,
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
            .review_signal_checks(|_| &[])
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
            .review_signal_checks(|_| &[])
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    )
}

fn revision_prompt(ws: &std::path::Path) -> String {
    compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&revision_task_with_created_via(None, "operator"))
            .workspace_path(ws)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    )
}

fn ci_fix_prompt(ws: &std::path::Path, failure_kind: &str, failed_checks: &str) -> String {
    let work_item = revision_task_with_created_via(None, "ci-fix:crm_frag_01");
    let mut attempt = sample_ci_attempt();
    attempt.failure_kind = Some(failure_kind.into());
    attempt.failed_checks = failed_checks.into();
    compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(ws)
            .ci_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    )
}

fn assert_no_bazel_gate_wording(prompt: &str) {
    let gated = without_cache_rule(prompt);
    assert!(!gated.contains("bazel build"), "bazel build leaked:\n{gated}");
    assert!(!gated.contains("bazel test"), "bazel test leaked:\n{gated}");
    assert!(!gated.contains("bazel mod"), "bazel mod leaked:\n{gated}");
    assert!(!gated.contains("bazel query"), "bazel query leaked:\n{gated}");
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
        prompt.contains("MUST run the repo's own documented compile, type-check and test commands"),
        "{prompt}"
    );
    assert!(prompt.contains("real exit code"), "{prompt}");
    assert!(
        !prompt.contains("The full test suite is not a precondition"),
        "conflict gate must not exempt tests:\n{prompt}"
    );
    assert_no_bazel_gate_wording(&conflict_prompt(ws.path()));
}

#[test]
fn npm_chore_gets_npm_gate_without_bazel_commands() {
    let ws = npm_workspace();
    let prompt = without_cache_rule(&chore_prompt(ws.path()));
    assert!(
        prompt.contains("## Pre-push build gate (non-Bazel repository)"),
        "{prompt}"
    );
    assert_no_bazel_gate_wording(&chore_prompt(ws.path()));
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

#[test]
fn npm_non_conflict_revision_gets_full_gate_and_rebase_clause() {
    let ws = npm_workspace();
    let prompt = without_cache_rule(&revision_prompt(ws.path()));
    assert!(
        prompt.contains("## Pre-push build gate (non-Bazel repository)"),
        "{prompt}"
    );
    assert!(prompt.contains("compile, type-check and test commands"), "{prompt}");
    assert!(
        prompt.contains("MUST run the repo's own documented compile, type-check and test commands before pushing"),
        "non-conflict rebase clause missing:\n{prompt}"
    );
    assert_no_bazel_gate_wording(&revision_prompt(ws.path()));
}

#[test]
fn no_marker_non_conflict_revision_reports_missing_command() {
    let ws = tempfile::TempDir::new().unwrap();
    let prompt = without_cache_rule(&revision_prompt(ws.path()));
    assert!(prompt.contains("No build-system manifest was detected"), "{prompt}");
    assert!(prompt.contains("no build command was found"), "{prompt}");
    assert!(
        prompt.contains("## Pre-push build gate (non-Bazel repository)"),
        "{prompt}"
    );
    assert_no_bazel_gate_wording(&revision_prompt(ws.path()));
}

#[test]
fn worker_signal_seam_off_uses_blocked_marker_wording() {
    let ws = tempfile::TempDir::new().unwrap();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&revision_task_with_created_via(None, "operator"))
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .worker_signal_proposals_seam_enabled(false)
            .build(),
    );
    assert!(
        prompt.contains("[blocked] reason="),
        "flag-off path must teach the marker:\n{prompt}"
    );
    assert!(
        !prompt.contains("propose blocked"),
        "flag-off path must not teach propose blocked:\n{prompt}"
    );
}

#[test]
fn test_script_only_repo_conflict_gate_requires_tests_and_exit_codes() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"test":"jest"}}"#.as_bytes(),
    )
    .unwrap();
    let prompt = without_cache_rule(&conflict_prompt(dir.path()));
    assert!(prompt.contains("`package.json` scripts"), "{prompt}");
    assert!(prompt.contains("npm test"), "{prompt}");
    assert!(prompt.contains("test commands"), "{prompt}");
    assert!(prompt.contains("real exit code"), "{prompt}");
    assert!(
        prompt.contains("A repo whose only documented check is a test command still requires that test"),
        "{prompt}"
    );
}

#[test]
fn bazel_override_on_missing_local_path_gets_bazel_gate_not_non_bazel() {
    let missing = std::path::Path::new("/this/remote/workspace/does-not-exist-locally");
    assert!(!missing.exists());
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(missing)
            .bazel_workspace(true)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    assert!(prompt.contains("## Pre-push build gate (Bazel workspace)"), "{prompt}");
    assert!(
        prompt.contains("bazel build") && prompt.contains("bazel test"),
        "{prompt}"
    );
    assert!(!prompt.contains("non-Bazel repository"), "{prompt}");
    assert!(!prompt.contains("do NOT run `bazel` commands"), "{prompt}");

    let revision = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&revision_task_with_created_via(None, "operator"))
            .workspace_path(missing)
            .bazel_workspace(true)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    assert!(
        revision.contains("## Pre-push build gate (Bazel workspace)"),
        "{revision}"
    );
    assert!(!revision.contains("non-Bazel repository"), "{revision}");
}

fn missing_path() -> &'static std::path::Path {
    let p = std::path::Path::new("/this/remote/workspace/does-not-exist-locally");
    assert!(!p.exists());
    p
}

fn assert_unresolved_gate_neutral(prompt: &str) {
    let gated = without_cache_rule(prompt);
    assert!(
        gated.contains("build system unresolved") || gated.contains("could not confirm this workspace's build system"),
        "expected discovery gate:\n{gated}"
    );
    assert!(
        gated.contains("no build command was found") || gated.contains("stop as blocked"),
        "expected blocked instruction:\n{gated}"
    );
    assert!(!gated.contains("do NOT run `bazel`"), "{gated}");
    assert!(!gated.contains("This repository is not a Bazel workspace"), "{gated}");
    assert!(
        !gated.contains("## Pre-push build gate (Bazel workspace)"),
        "Bazel-only gate leaked:\n{gated}"
    );
    assert!(
        !gated.contains("## Pre-push build gate (non-Bazel repository)"),
        "non-Bazel gate leaked:\n{gated}"
    );
    assert!(
        !gated.contains("## Pre-push gate for conflict resolution (Bazel workspace)"),
        "Bazel-only conflict gate leaked:\n{gated}"
    );
    assert!(
        !gated.contains("## Pre-push gate for conflict resolution (non-Bazel repository)"),
        "non-Bazel conflict gate leaked:\n{gated}"
    );
}

fn queue_ci_section(prompt: &str) -> String {
    prompt
        .split("## Merge-queue CI verification")
        .nth(1)
        .unwrap_or("")
        .to_owned()
}

#[test]
fn missing_local_path_without_override_is_not_treated_as_non_bazel() {
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(missing_path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    assert_unresolved_gate_neutral(&prompt);
}

#[test]
fn npm_ci_fix_trunk_eviction_empty_checks_uses_job_invocation() {
    let ws = npm_workspace();
    let prompt = without_cache_rule(&ci_fix_prompt(ws.path(), "trunk_queue_eviction", "[]"));
    assert!(prompt.contains("non-Bazel repository"), "{prompt}");
    assert!(prompt.contains("job's own documented invocation"), "{prompt}");
    assert!(!prompt.contains("run every failed target with Bazel"), "{prompt}");
    assert_no_bazel_gate_wording(&ci_fix_prompt(ws.path(), "trunk_queue_eviction", "[]"));
}

#[test]
fn npm_merge_queue_ci_fix_requires_documented_invocation() {
    let ws = npm_workspace();
    let prompt = without_cache_rule(&ci_fix_prompt(
        ws.path(),
        "merge_queue_rebounce",
        r#"[{"name":"frontend-test","conclusion":"FAILURE","target_url":"https://buildkite.com/o/p/builds/1"}]"#,
    ));
    assert!(prompt.contains("job's own documented invocation"), "{prompt}");
    assert!(
        prompt.contains("<the failing job's documented invocation, copied in full>"),
        "{prompt}"
    );
    assert!(prompt.contains("real exit code"), "{prompt}");
    assert!(!prompt.contains("bazel test //path:each_failed_target"), "{prompt}");
    assert!(
        !prompt.contains("use `bazel test` for EVERY failed test target"),
        "{prompt}"
    );
}

#[test]
fn unresolved_conflict_revision_gets_discovery_gate() {
    assert_unresolved_gate_neutral(&conflict_prompt(missing_path()));
}

#[test]
fn unresolved_non_conflict_revision_gets_discovery_gate() {
    assert_unresolved_gate_neutral(&revision_prompt(missing_path()));
}

#[test]
fn unresolved_trunk_eviction_ci_fix_gets_discovery_gate() {
    let prompt = ci_fix_prompt(missing_path(), "trunk_queue_eviction", "[]");
    assert_unresolved_gate_neutral(&prompt);
    let gated = without_cache_rule(&prompt);
    assert!(
        gated.contains("identify the job's build system") || gated.contains("stop as blocked"),
        "{gated}"
    );
}

#[test]
fn unresolved_queue_ci_prompt_is_build_system_neutral() {
    let prompt = ci_fix_prompt(
        missing_path(),
        "merge_queue_rebounce",
        r#"[{"name":"frontend-test","conclusion":"FAILURE","target_url":"https://buildkite.com/o/p/builds/1"}]"#,
    );
    assert_unresolved_gate_neutral(&prompt);
    let section = queue_ci_section(&prompt);
    assert!(
        section.contains("identify the build system") || section.contains("stop as blocked"),
        "{section}"
    );
    assert!(!section.contains("do NOT run `bazel`"), "{section}");
    assert!(!section.contains("Do not substitute `bazel` commands"), "{section}");
}

#[test]
fn bazel_override_ci_fix_keeps_bazel_test_step3() {
    let work_item = revision_task_with_created_via(None, "ci-fix:crm_frag_01");
    let mut attempt = sample_ci_attempt();
    attempt.failure_kind = Some("merge_queue_rebounce".into());
    attempt.failed_checks =
        r#"[{"name":"bazel-build-test","conclusion":"FAILURE","target_url":"https://buildkite.com/o/p/builds/1"}]"#
            .into();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .review_signal_checks(|_| &[])
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(missing_path())
            .bazel_workspace(true)
            .ci_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    let section = queue_ci_section(&prompt);
    assert!(
        section.contains("use `bazel test` for EVERY failed test target"),
        "{section}"
    );
    assert!(section.contains("bazel test //path:each_failed_target"), "{section}");
}

#[test]
fn npm_queue_ci_prompt_advertises_only_validator_accepted_commands() {
    let prompt = without_cache_rule(&ci_fix_prompt(
        npm_workspace().path(),
        "merge_queue_rebounce",
        r#"[{"name":"frontend-test","conclusion":"FAILURE","target_url":"https://buildkite.com/o/p/builds/1"}]"#,
    ));
    let section = queue_ci_section(&prompt);
    assert!(
        section.contains("npm") || section.contains("pnpm") || section.contains("yarn"),
        "{section}"
    );
    assert!(section.contains("BOSS_CI_VERIFICATION.json"), "{section}");
    assert!(!section.contains("`cargo test`"), "{section}");
    assert!(!section.contains("`go test"), "{section}");
    assert!(
        section.contains("stop as blocked") || section.contains("are rejected"),
        "{section}"
    );
}
