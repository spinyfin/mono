//! Shape-based coverage that worker prompts never instruct a bare `boss` /
//! `cube` PATH lookup. Split out of `compose_prompt_tests.rs` so that file
//! stays under the `file/size` cap.

use super::{
    ExecutionPromptParams, base_execution, bazel_workspace, chore_with_pr, chore_without_pr, compose_execution_prompt,
    design_task, revision_execution, revision_task_with_created_via, sample_conflict_attempt,
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
            .worker_signal_proposals_seam_enabled(true)
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

/// `run_done_proposals_seam` and `worker_signal_proposals_seam` are independently
/// defaulted off. Enabling only the former must still teach `propose done`,
/// but must not teach `propose blocked` — that verb is the latter flag's job.
/// The legacy `[blocked]` marker is Stop-boundary-only and cannot record a
/// blocker emitted immediately before the terminal `propose done` call, so
/// this mixed-flag path preserves the reason on `--summary` instead.
#[test]
fn run_done_seam_on_worker_signal_seam_off_teaches_summary_not_blocked_verb() {
    let ws = bazel_workspace();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    assert!(
        prompt.contains("## Declaring your run finished"),
        "run_done seam on: must still teach the terminal declaration:\n{prompt}",
    );
    assert!(
        prompt.contains("`\"$BOSS_BIN\" propose done"),
        "run_done seam on: must teach the propose done verb:\n{prompt}",
    );
    assert!(
        !prompt.contains("propose blocked"),
        "worker_signal seam off: run_done directive must not teach propose blocked:\n{prompt}",
    );
    assert!(
        !prompt.contains("a `[blocked] reason=\"...\"` marker alone records"),
        "worker_signal seam off: must not claim the marker records synchronously:\n{prompt}",
    );
    assert!(
        !prompt.contains("Emit a `[blocked] reason=\"...\"` marker alongside it"),
        "worker_signal seam off: must not instruct emitting the marker immediately before propose done:\n{prompt}",
    );
    assert!(
        prompt.contains("Put the blocker reason in this call's `--summary`"),
        "worker_signal seam off: blocked outcome must preserve the reason on propose done --summary:\n{prompt}",
    );
    assert!(
        prompt.contains("the marker is Stop-boundary-only"),
        "worker_signal seam off: must describe the marker as Stop-boundary-only:\n{prompt}",
    );
    assert!(
        prompt.contains(
            "a `[blocked] reason=\"...\"` marker is parsed only at the Stop boundary (when the turn \
             ends without a terminal `propose done`) and records"
        ),
        "worker_signal seam off: while-continuing must teach Stop-boundary-only marker parsing:\n{prompt}",
    );
}

#[test]
fn run_done_seam_on_worker_signal_seam_on_teaches_propose_blocked() {
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .worker_signal_proposals_seam_enabled(true)
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    assert!(
        prompt.contains("`\"$BOSS_BIN\" propose blocked --reason \"...\"` alone records"),
        "both seams on: run_done directive must teach propose blocked for the while-continuing case:\n{prompt}",
    );
    assert!(
        !prompt.contains("a `[blocked] reason=\"...\"` marker alone records"),
        "both seams on: run_done directive must not fall back to the marker as the primary channel:\n{prompt}",
    );
}

/// When `run_done_proposals_seam` is on, the Bazel pre-push gate itself must
/// override its absolute "do not push red code" stop with the evidence-gated
/// unattributable-failure exception — not leave that exception only in the
/// later run-done section about which terminal outcome to declare.
#[test]
fn bazel_prepush_gate_allows_evidenced_unattributable_push_when_run_done_seam_on() {
    let ws = bazel_workspace();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    let gate = prompt
        .split("## Declaring your run finished")
        .next()
        .expect("run_done section must follow the gate");
    assert!(
        gate.contains("## Pre-push build gate (Bazel workspace)"),
        "chore on a Bazel workspace must still render the pre-push gate:\n{gate}",
    );
    assert!(
        gate.contains("You MAY push with a still-red, evidence-backed-unattributable target"),
        "run_done seam on: the gate itself must authorize an evidenced unattributable push:\n{gate}",
    );
    assert!(
        gate.contains("unless a still-red target is evidence-backed as pre-existing or environmental"),
        "run_done seam on: the clean-finish bullet must name the exception, not stay absolute:\n{gate}",
    );
    assert!(
        !gate.contains("do NOT push red code and do NOT idle waiting on them"),
        "run_done seam on: the gate must not keep the absolute do-not-push-red-code stop:\n{gate}",
    );
    assert!(
        gate.contains("If the failure is attributable to your change and you cannot fix it, do NOT push red code"),
        "run_done seam on: attributable failures must still forbid the push:\n{gate}",
    );
    assert!(
        prompt.contains("You MAY push despite a still-red target, and declare `delivered`"),
        "run_done directive must also state the push is allowed, not only the terminal outcome:\n{prompt}",
    );
}

#[test]
fn bazel_prepush_gate_keeps_absolute_stop_when_run_done_seam_off() {
    let ws = bazel_workspace();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("do NOT push red code and do NOT idle waiting on them"),
        "run_done seam off: the gate must keep the absolute do-not-push-red-code stop:\n{prompt}",
    );
    assert!(
        !prompt.contains("You MAY push with a still-red, evidence-backed-unattributable target"),
        "run_done seam off: the gate must not teach the unattributable-push exception:\n{prompt}",
    );
}

#[test]
fn revision_prepush_gate_allows_evidenced_unattributable_push_when_run_done_seam_on() {
    let ws = bazel_workspace();
    let work_item = revision_task_with_created_via(None, "operator");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    let gate = prompt
        .split("## Declaring your run finished")
        .next()
        .expect("run_done section must follow the revision gate");
    assert!(
        gate.contains("You MAY push with a still-red, evidence-backed-unattributable target"),
        "run_done seam on: a non-conflict revision gate must authorize an evidenced unattributable push:\n{gate}",
    );
    assert!(
        !prompt.contains("propose blocked"),
        "worker_signal seam off: revision run_done text must not teach propose blocked:\n{prompt}",
    );
}

/// Conflict-resolution revisions use `bazel_conflict_resolution_gate_text`,
/// which never receives the unattributable-failure exception. With the
/// run_done seam on, that gate's absolute compile-or-do-not-push wording
/// must survive verbatim, and `run_done_directive` must not append the
/// generic MAY-push exception that would read as overriding it.
#[test]
fn conflict_revision_keeps_absolute_compile_stop_when_run_done_seam_on() {
    let ws = bazel_workspace();
    let work_item = revision_task_with_created_via(None, "merge-conflict:crz_frag_01");
    let attempt = sample_conflict_attempt();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(ws.path())
            .conflict_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .run_done_proposals_seam_enabled(true)
            .build(),
    );
    let gate = prompt
        .split("## Declaring your run finished")
        .next()
        .expect("run_done section must follow the conflict gate");
    assert!(
        gate.contains("## Pre-push gate for conflict resolution (Bazel workspace)"),
        "conflict revision must still get the merge-correctness gate with run_done seam on:\n{gate}",
    );
    assert!(
        gate.contains("The merged code MUST COMPILE"),
        "run_done seam on: conflict gate must still require a clean build verbatim:\n{gate}",
    );
    assert!(
        gate.contains(
            "If `bazel build` fails or times out, do NOT push. A successful build is required before delivery."
        ),
        "run_done seam on: conflict gate's absolute do-NOT-push wording must be preserved verbatim:\n{gate}",
    );
    assert!(
        !gate.contains("You MAY push with a still-red, evidence-backed-unattributable target"),
        "run_done seam on: the conflict gate itself must not grow the unattributable-push exception:\n{gate}",
    );
    assert!(
        !prompt.contains("You MAY push despite a still-red target"),
        "run_done seam on: conflict run_done text must not authorize pushing a still-red compile:\n{prompt}",
    );
    assert!(
        !prompt.contains("including the pre-push Bazel gate"),
        "run_done seam on: conflict run_done text must not refer generically to the pre-push Bazel gate:\n{prompt}",
    );
    assert!(
        prompt.contains("The merge-correctness pre-push gate is not covered by any unattributable-failure exception."),
        "run_done seam on: conflict run_done text must explicitly except the merge-correctness gate:\n{prompt}",
    );
    assert!(
        prompt.contains("the merged code MUST COMPILE"),
        "run_done seam on: conflict run_done text must restate MUST COMPILE:\n{prompt}",
    );
    let run_done = prompt.split("## Declaring your run finished").nth(1).unwrap();
    assert!(
        run_done.contains("If `bazel build` fails or times out, do NOT push"),
        "conflict run_done text must prohibit pushing failed or timed-out builds:\n{run_done}",
    );
    assert!(
        run_done.contains("Only the full test suite is deferred to CI"),
        "conflict run_done text must limit CI deferral to tests:\n{run_done}",
    );
    assert!(
        !prompt.contains("Timeouts and the full test suite are not a precondition"),
        "conflict prompt must not exempt build timeouts from the compile gate:\n{prompt}",
    );
}
