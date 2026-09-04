//! Shape-based coverage that worker prompts never instruct a bare `boss` /
//! `cube` PATH lookup. Split out of `compose_prompt_tests.rs` so that file
//! stays under the `file/size` cap.

use super::{
    ExecutionPromptParams, base_execution, chore_with_pr, chore_without_pr, compose_execution_prompt, design_task,
    revision_execution, revision_task_with_created_via, sample_conflict_attempt,
};
use boss_protocol::ExecutionKind;

/// Reject backtick-wrapped `boss ` / `cube ` invocations. The PreToolUse
/// launch guard blocks on the first token, so any remaining verb (not just
/// `propose` / `pr`) wastes a round trip. Shape-based so a new verb cannot
/// slip through the way `boss project set-design-doc` did.
fn assert_no_bare_path_binary_invocations(prompt: &str, label: &str) {
    for prefix in ["`boss ", "`cube "] {
        assert!(
            !prompt.contains(prefix),
            "{label}: prompt must not instruct workers to invoke a bare-path binary ({prefix}):\n{prompt}",
        );
    }
}

fn parent_project_without_design_doc() -> crate::work::Project {
    crate::work::Project::builder()
        .id("proj-1")
        .product_id("prod-1")
        .name("My Project")
        .description("")
        .goal("")
        .status(crate::work::ProjectStatus::Active)
        .slug("my-project")
        .created_at("2026-05-15T00:00:00Z")
        .updated_at("2026-05-15T00:00:00Z")
        .build()
}

#[test]
fn rendered_prompt_uses_engine_owned_binary_invocations() {
    let parent_project = parent_project_without_design_doc();
    assert!(
        parent_project.design_doc_path.is_none(),
        "fixture must omit design_doc_path so the set-design-doc instruction is rendered",
    );

    let mut design_exec = base_execution();
    design_exec.kind = ExecutionKind::ProjectDesign;
    let design_item = design_task();
    let existing_pr = chore_with_pr("https://github.com/org/repo/pull/42");
    let conflict_item = revision_task_with_created_via(
        Some("https://github.com/org/repo/pull/77"),
        "merge-conflict:crz_frag_01",
    );
    let conflict_attempt = sample_conflict_attempt();

    let chore_prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    let design_prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&design_exec)
            .work_item(&design_item)
            .parent_project(&parent_project)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    let existing_pr_prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&existing_pr)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    let conflict_prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&conflict_item)
            .conflict_attempt(&conflict_attempt)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );

    assert!(
        chore_prompt.contains("`\"$BOSS_BIN\" propose done"),
        "run-done command must use the engine-owned boss binary:\n{chore_prompt}",
    );
    assert!(
        chore_prompt.contains("`\"$BOSS_BIN\" propose blocked"),
        "blocked command must use the engine-owned boss binary:\n{chore_prompt}",
    );
    assert!(
        chore_prompt.contains("`\"$CUBE_BIN\" pr create` / `\"$CUBE_BIN\" pr update`"),
        "terminal-push command must use the engine-owned cube binary:\n{chore_prompt}",
    );

    let boss = boss_engine_worker_bin::WORKER_BOSS_INVOCATION;
    assert!(
        design_prompt.contains(&format!("`{boss} project set-design-doc")),
        "design prompt with no design_doc_path must teach the engine-owned set-design-doc invocation:\n{design_prompt}",
    );
    assert!(
        existing_pr_prompt.contains("## RESUME EXISTING PR"),
        "existing-PR variant must include the resume block:\n{existing_pr_prompt}",
    );
    assert!(
        conflict_prompt.contains("## Conflict resolution context"),
        "conflict-resolution variant must include the conflict fragment:\n{conflict_prompt}",
    );

    for (label, prompt) in [
        ("chore without PR", chore_prompt.as_str()),
        ("design without design_doc_path", design_prompt.as_str()),
        ("existing PR URL", existing_pr_prompt.as_str()),
        ("conflict-resolution revision", conflict_prompt.as_str()),
    ] {
        assert_no_bare_path_binary_invocations(prompt, label);
    }
}
