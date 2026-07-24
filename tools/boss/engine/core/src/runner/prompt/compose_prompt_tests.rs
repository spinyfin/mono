#![cfg(test)]
//! Tests for the worker prompt composition family.

use super::super::work_item::{extract_pr_url_from_text, task_bound_pr_url};
use boss_protocol::{ExecutionStatus, TaskStatus};

use super::*;
use crate::work::Task;

fn base_execution() -> WorkExecution {
    WorkExecution::builder()
        .id("exec_abc123_01")
        .work_item_id("task-1")
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@github.com:org/repo.git")
        .workspace_path("/tmp/workspace")
        .created_at("2026-05-15T00:00:00Z")
        .build()
}

fn chore_without_pr() -> WorkItem {
    WorkItem::Chore(
        Task::builder()
            .id("task-1")
            .product_id("prod-1")
            .kind(TaskKind::Chore)
            .name("Fix the thing")
            .description("Description here.")
            .status(TaskStatus::Todo)
            .created_at("2026-05-15T00:00:00Z")
            .updated_at("2026-05-15T00:00:00Z")
            .autostart(false)
            .build(),
    )
}

fn chore_with_pr(pr_url: &str) -> WorkItem {
    match chore_without_pr() {
        WorkItem::Chore(mut task) => {
            task.pr_url = Some(pr_url.into());
            WorkItem::Chore(task)
        }
        other => other,
    }
}

#[test]
fn no_resume_directive_when_pr_url_is_absent() {
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("RESUME EXISTING PR"),
        "should have no resume block when pr_url is None:\n{prompt}",
    );
}

#[test]
fn no_resume_directive_when_pr_url_is_empty() {
    let chore = chore_with_pr("");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("RESUME EXISTING PR"),
        "should have no resume block when pr_url is empty:\n{prompt}",
    );
}

#[test]
fn resume_directive_present_when_pr_url_is_set() {
    let chore = chore_with_pr("https://github.com/org/repo/pull/42");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("## RESUME EXISTING PR"),
        "missing resume block when pr_url is set:\n{prompt}",
    );
    assert!(
        prompt.contains("https://github.com/org/repo/pull/42"),
        "resume block should include the PR URL:\n{prompt}",
    );
    assert!(
        prompt.contains("#42"),
        "resume block should include the PR number:\n{prompt}",
    );
}

#[test]
fn resume_directive_appears_before_execution_context() {
    let chore = chore_with_pr("https://github.com/org/repo/pull/99");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    let resume_pos = prompt.find("## RESUME EXISTING PR").expect("missing resume block");
    let exec_pos = prompt.find("Execution context:").expect("missing execution context");
    assert!(
        resume_pos < exec_pos,
        "resume block must appear before execution context:\n{prompt}",
    );
}

#[test]
fn expected_branch_name_suppressed_when_pr_url_set() {
    let chore = chore_with_pr("https://github.com/org/repo/pull/42");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("expected branch name"),
        "expected-branch-name line should be suppressed when resuming a PR:\n{prompt}",
    );
}

#[test]
fn expected_branch_name_present_when_no_pr_url() {
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("expected branch name"),
        "expected-branch-name line must be present for fresh dispatches:\n{prompt}",
    );
}

#[test]
fn acceptance_criterion_references_existing_pr_when_pr_url_set() {
    let chore = chore_with_pr("https://github.com/org/repo/pull/42");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("Do NOT open a new PR"),
        "acceptance criterion should prohibit opening a new PR:\n{prompt}",
    );
    assert!(
        prompt.contains("gh pr view 42"),
        "acceptance criterion should reference gh pr view for the existing PR:\n{prompt}",
    );
}

#[test]
fn acceptance_criterion_uses_fresh_branch_when_no_pr_url() {
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("jj bookmark create"),
        "acceptance criterion should guide fresh branch creation:\n{prompt}",
    );
    assert!(
        prompt.contains("gh pr create") || prompt.contains("cube pr create"),
        "acceptance criterion should guide opening a new PR:\n{prompt}",
    );
}

#[test]
fn no_recovery_block_when_no_prior_branch() {
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("STARTUP RECOVERY"),
        "no recovery block expected when recovery_branch is None:\n{prompt}",
    );
}

#[test]
fn recovery_block_injected_when_prior_branch_provided() {
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .recovery_branch("boss/exec_prior123_09")
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("## STARTUP RECOVERY"),
        "recovery block should be present when recovery_branch is Some:\n{prompt}",
    );
    assert!(
        prompt.contains("boss/exec_prior123_09"),
        "recovery block should name the prior branch:\n{prompt}",
    );
    assert!(
        prompt.contains("jj edit boss/exec_prior123_09@origin"),
        "recovery block should instruct jj edit on the remote branch:\n{prompt}",
    );
}

// ── STARTUP RECOVERY: recovered working state (P3) ────────────────────
//
// The old block talked only about a *pushed branch* and told the worker
// to fall back to `jj new main@origin` — which moves `@` off any
// recovered uncommitted state. These tests pin the fix.

fn apply_report(paths: &[&str], insertions: usize, deletions: usize) -> crate::recovery_apply::ApplyReport {
    crate::recovery_apply::ApplyReport {
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        insertions,
        deletions,
        filtered_paths: vec![".boss/events-pending.jsonl".to_owned()],
    }
}

/// With no recovery marker the block must still refuse the unconditional
/// `jj new main@origin` that used to discard recovered state.
#[test]
fn recovery_block_never_tells_the_worker_to_unconditionally_reset() {
    let block = startup_recovery_block("boss/exec_prior_09", "boss/exec_new_01", None);
    assert!(
        block.contains("Do NOT run `jj new main@origin`"),
        "the block must forbid the reset that discards recovered state:\n{block}",
    );
    assert!(
        block.contains("Only if `jj status` shows the working copy is **clean**"),
        "the reset must be conditional on a clean working copy:\n{block}",
    );
}

/// Cube recovered the tree in place: say so, say the jj history came with
/// it, and tell the worker to look before it leaps.
#[test]
fn recovery_block_reports_in_place_recovery() {
    let report = crate::recovery_apply::RecoveryReport {
        for_execution_id: "exec_new".to_owned(),
        from_execution_id: "exec_dead".to_owned(),
        source: crate::recovery_apply::RecoverySource::CubeInPlace,
        applied: None,
        patch_error: None,
    };
    let block = startup_recovery_block("boss/exec_prior_09", "boss/exec_new_01", Some(&report));
    assert!(block.contains("State recovered IN PLACE"), "{block}");
    assert!(
        block.contains("operation log is intact") || block.contains("operation log"),
        "{block}"
    );
    assert!(block.contains("Do not reset it."), "{block}");
    assert!(
        block.contains("jj diff --stat"),
        "the worker must be told how to inspect before building on it:\n{block}",
    );
}

/// Patch recovery: name the files and the line counts, and be explicit
/// that only uncommitted edits came across.
#[test]
fn recovery_block_reports_patch_recovery_in_human_terms() {
    let report = crate::recovery_apply::RecoveryReport {
        for_execution_id: "exec_new".to_owned(),
        from_execution_id: "exec_dead".to_owned(),
        source: crate::recovery_apply::RecoverySource::Patch,
        applied: Some(apply_report(&["tools/cube/src/app.rs", "docs/x.md"], 120, 14)),
        patch_error: None,
    };
    let block = startup_recovery_block("boss/exec_prior_09", "boss/exec_new_01", Some(&report));
    assert!(block.contains("State recovered FROM A PATCH"), "{block}");
    assert!(block.contains("2 file(s), +120/-14"), "{block}");
    assert!(block.contains("`tools/cube/src/app.rs`"), "{block}");
    assert!(block.contains("`docs/x.md`"), "{block}");
    assert!(
        block.contains("uncommitted edits only"),
        "the worker must know the jj history did NOT come with the patch:\n{block}",
    );
    assert!(block.contains("Do not reset the working copy."), "{block}");
}

/// A failed apply must be surfaced to the worker, not silently omitted.
/// Silence would leave it believing it was resuming.
#[test]
fn recovery_block_says_so_loudly_when_the_patch_did_not_apply() {
    let report = crate::recovery_apply::RecoveryReport {
        for_execution_id: "exec_new".to_owned(),
        from_execution_id: "exec_dead".to_owned(),
        source: crate::recovery_apply::RecoverySource::Patch,
        applied: None,
        patch_error: Some("error: patch does not apply".to_owned()),
    };
    let block = startup_recovery_block("boss/exec_prior_09", "boss/exec_new_01", Some(&report));
    assert!(block.contains("Recovery FAILED"), "{block}");
    assert!(block.contains("error: patch does not apply"), "{block}");
    assert!(
        block.contains("Do NOT assume any of the prior work is present."),
        "the worker must not believe it is resuming:\n{block}",
    );
    assert!(
        !block.contains("State recovered"),
        "a failed recovery must never read as a successful one:\n{block}",
    );
}

/// The pushed-branch half of the block is preserved in every variant —
/// the new working-state reporting is additive, not a replacement.
#[test]
fn recovery_block_keeps_the_prior_branch_instructions() {
    for report in [
        None,
        Some(crate::recovery_apply::RecoveryReport {
            for_execution_id: "e".to_owned(),
            from_execution_id: "d".to_owned(),
            source: crate::recovery_apply::RecoverySource::CubeInPlace,
            applied: None,
            patch_error: None,
        }),
    ] {
        let block = startup_recovery_block("boss/exec_prior_09", "boss/exec_new_01", report.as_ref());
        assert!(block.contains("jj edit boss/exec_prior_09@origin"), "{block}");
        assert!(block.contains("boss/exec_new_01"), "{block}");
    }
}

#[test]
fn recovery_block_suppressed_when_pr_url_set() {
    // When the work item already has a PR URL, the existing RESUME
    // EXISTING PR path takes precedence; the recovery block must not
    // also appear (that would be contradictory).
    let chore = chore_with_pr("https://github.com/org/repo/pull/42");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .recovery_branch("boss/exec_prior123_09")
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("STARTUP RECOVERY"),
        "recovery block must not appear when existing PR URL takes precedence:\n{prompt}",
    );
    assert!(
        prompt.contains("## RESUME EXISTING PR"),
        "RESUME EXISTING PR block should still be present:\n{prompt}",
    );
}

#[test]
fn recovery_block_appears_before_execution_context() {
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .recovery_branch("boss/exec_prior123_09")
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    let recovery_pos = prompt.find("## STARTUP RECOVERY").expect("missing recovery block");
    let exec_pos = prompt.find("Execution context:").expect("missing execution context");
    assert!(
        recovery_pos < exec_pos,
        "recovery block must appear before execution context:\n{prompt}",
    );
}

#[test]
fn recovery_block_mentions_new_expected_branch() {
    // The new worker should push under the NEW expected branch name
    // (derived from the current execution id), not the prior one.
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .recovery_branch("boss/exec_prior123_09")
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    // "boss/exec_abc123_01" is the new expected branch
    assert!(
        prompt.contains("boss/exec_abc123_01"),
        "recovery block should mention the new expected branch name:\n{prompt}",
    );
}

/// Like `base_execution` but pointed at a given repo remote, so the
/// CI-monitoring directive's org-specific branch can be exercised.
fn execution_for_remote(remote: &str) -> WorkExecution {
    WorkExecution::builder()
        .id("exec_abc123_01")
        .work_item_id("task-1")
        .kind(ExecutionKind::ChoreImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url(remote.to_string())
        .workspace_path("/tmp/workspace")
        .created_at("2026-05-15T00:00:00Z")
        .build()
}

#[test]
fn ci_monitoring_directive_present_for_implementation_chore() {
    // Issue #899: the worker must be told not to poll CI forever, and
    // that the engine auto-transitions to Review once CI is effectively
    // green. This general guidance applies regardless of org.
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("do not babysit CI"),
        "missing CI-monitoring directive:\n{prompt}",
    );
    assert!(
        prompt.contains("effectively green"),
        "directive should reference the engine's effectively-green definition:\n{prompt}",
    );
}

#[test]
fn ci_monitoring_directive_names_human_gated_checks_for_linkedin_org() {
    // The human-gated check name must be sourced from the engine's
    // REVIEW_SIGNAL_RULES table (via review_signal_checks_for_owner),
    // not re-hardcoded in the prompt — single sourcing is the fix.
    let exec = execution_for_remote("git@github.com:linkedin-multiproduct/some-repo.git");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&exec)
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("Owner Approval"),
        "directive should name the org's human-gated check:\n{prompt}",
    );
    assert!(
        prompt.contains("linkedin-multiproduct"),
        "directive should name the org:\n{prompt}",
    );
}

#[test]
fn ci_monitoring_directive_omits_human_gated_names_for_plain_org() {
    // A non-LinkedIn org has no review-signal rules; the directive's
    // general guidance stands alone without naming any check.
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&execution_for_remote("git@github.com:org/repo.git"))
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("do not babysit CI"),
        "general directive should still be present:\n{prompt}",
    );
    assert!(
        !prompt.contains("Owner Approval"),
        "no human-gated check should be named for a plain org:\n{prompt}",
    );
}

#[test]
fn no_op_directive_present_for_fresh_chore_without_pr() {
    // T1868: a fresh chore_implementation worker (no existing PR) must
    // be told the sanctioned way to terminate when the work is already
    // done — emit NO_CHANGES_NEEDED — instead of only "stop and explain".
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains(crate::no_op_signal::NO_CHANGES_NEEDED_MARKER),
        "fresh-chore prompt must name the NO_CHANGES_NEEDED marker:\n{prompt}",
    );
    assert!(
        prompt.contains("signal a sanctioned no-op"),
        "fresh-chore prompt must carry the no-op completion directive:\n{prompt}",
    );
}

#[test]
fn no_op_directive_absent_when_pr_already_exists() {
    // When a PR already exists (resume / existing-PR flow), an empty diff
    // means "already pushed" and is handled by the push-to-existing path
    // — NOT by closing the task as a no-op. The directive must not appear.
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_with_pr("https://github.com/org/repo/pull/7"))
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains(crate::no_op_signal::NO_CHANGES_NEEDED_MARKER),
        "existing-PR prompt must NOT carry the no-op marker directive:\n{prompt}",
    );
}

#[test]
fn work_item_pr_url_returns_none_for_project() {
    let project = WorkItem::Project(
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
            .build(),
    );
    assert!(work_item_pr_url(&project).is_none());
}

// `extract_pr_number` was removed in favour of the shared
// `boss_github::pr_url::pr_number_from_url`; its canonical-parse and
// malformed-rejection cases are covered by that helper's own unit tests
// in `tools/boss/github/src/pr_url.rs`.

#[test]
fn extract_pr_url_from_text_finds_bare_url() {
    let s = "see https://github.com/org/repo/pull/42 for context";
    assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/42"),);
}

#[test]
fn extract_pr_url_from_text_strips_trailing_punctuation() {
    let s = "follow-up on https://github.com/org/repo/pull/42.";
    assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/42"),);
}

#[test]
fn extract_pr_url_from_text_strips_subpath() {
    let s = "see https://github.com/org/repo/pull/42/files";
    assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/42"),);
}

#[test]
fn extract_pr_url_from_text_handles_markdown_link() {
    let s = "[PR](https://github.com/org/repo/pull/7) is in review";
    assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/7"),);
}

#[test]
fn extract_pr_url_from_text_returns_none_for_issue_url() {
    let s = "> Imported from https://github.com/org/repo/issues/742";
    assert_eq!(extract_pr_url_from_text(s), None);
}

#[test]
fn extract_pr_url_from_text_returns_none_for_no_url() {
    assert_eq!(extract_pr_url_from_text("just a #235 reference"), None);
    assert_eq!(extract_pr_url_from_text(""), None);
}

#[test]
fn extract_pr_url_from_text_returns_none_when_two_distinct_prs() {
    // Two distinct PR URLs in the same text — abort rather than
    // guess; the worker is safer in the new-PR flow than bound to
    // the wrong existing PR.
    let s = "rebase https://github.com/org/repo/pull/10 onto https://github.com/org/repo/pull/20";
    assert_eq!(extract_pr_url_from_text(s), None);
}

#[test]
fn extract_pr_url_from_text_dedupes_same_url() {
    // The same PR mentioned twice (once bare, once with /files) is
    // still one match.
    let s = "PR https://github.com/org/repo/pull/42 also at https://github.com/org/repo/pull/42/files";
    assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/42"),);
}

#[test]
fn task_bound_pr_url_prefers_explicit_column() {
    let chore = chore_with_pr("https://github.com/org/repo/pull/99");
    let task = match &chore {
        WorkItem::Chore(t) => t,
        _ => unreachable!(),
    };
    assert_eq!(task_bound_pr_url(task), Some("https://github.com/org/repo/pull/99"),);
}

#[test]
fn task_bound_pr_url_returns_none_when_description_has_only_issue_url() {
    let chore = match chore_without_pr() {
        WorkItem::Chore(mut task) => {
            task.description = "> Imported from https://github.com/org/repo/issues/742".into();
            WorkItem::Chore(task)
        }
        other => other,
    };
    let task = match &chore {
        WorkItem::Chore(t) => t,
        _ => unreachable!(),
    };
    assert!(task_bound_pr_url(task).is_none());
}

#[test]
fn task_bound_pr_url_ignores_pr_url_in_description() {
    // Regression for T683 / exec_18b341df81251750_4: a chore imported
    // from an issue whose body *mentions* a PR URL (e.g. as a repro
    // example) must NOT cause a RESUME EXISTING PR block. The structured
    // `pr_url` field is the only authoritative source.
    let chore = match chore_without_pr() {
        WorkItem::Chore(mut task) => {
            task.description =
                "Parent chore C19 landed at https://github.com/linkedin-multiproduct/dev-infra/pull/250 \
                     as a repro example — this chore has no PR yet."
                    .into();
            WorkItem::Chore(task)
        }
        other => other,
    };
    let task = match &chore {
        WorkItem::Chore(t) => t,
        _ => unreachable!(),
    };
    assert!(
        task_bound_pr_url(task).is_none(),
        "description-embedded PR URL must not be treated as the task's PR",
    );
}

#[test]
fn resume_directive_absent_when_pr_url_is_null() {
    // Regression for T683: a chore with pr_url=null and a description
    // mentioning a PR must not generate a RESUME EXISTING PR block.
    let chore = match chore_without_pr() {
        WorkItem::Chore(mut task) => {
            task.description = "Ref: https://github.com/linkedin-multiproduct/dev-infra/pull/250".into();
            WorkItem::Chore(task)
        }
        other => other,
    };
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("## RESUME EXISTING PR"),
        "RESUME block must NOT fire when task.pr_url is null, even if description mentions a PR:\n{prompt}",
    );
}

#[test]
fn resume_directive_present_when_structured_pr_url_is_set() {
    // Positive case: task with an explicit pr_url gets the RESUME block.
    let chore = chore_with_pr("https://github.com/org/repo/pull/235");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("## RESUME EXISTING PR"),
        "resume block must fire when task.pr_url is set:\n{prompt}",
    );
    assert!(
        prompt.contains("https://github.com/org/repo/pull/235"),
        "resume block must quote the structured PR URL:\n{prompt}",
    );
    assert!(
        prompt.contains("#235"),
        "resume block must surface the PR number:\n{prompt}",
    );
}

fn revision_execution(pr_url: &str) -> WorkExecution {
    WorkExecution::builder()
        .id("exec_rev_01")
        .work_item_id("task-1")
        .kind(ExecutionKind::RevisionImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@github.com:org/repo.git")
        .workspace_path("/tmp/workspace")
        .pr_url(pr_url)
        .created_at("2026-05-15T00:00:00Z")
        .build()
}

/// Lay down a `MODULE.bazel` marker so `is_bazel_workspace` treats
/// the tempdir as a Bazel workspace (issue #804).
fn bazel_workspace() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();
    dir
}

#[test]
fn bazel_gate_present_for_chore_on_bazel_workspace_seam_on() {
    let ws = bazel_workspace();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .worker_signal_proposals_seam_enabled(true)
            .build(),
    );
    assert!(
        prompt.contains("## Pre-push build gate (Bazel workspace)"),
        "bazel pre-push gate must fire for code chores on a Bazel workspace:\n{prompt}",
    );
    assert!(
        prompt.contains("bazel build") && prompt.contains("bazel test"),
        "gate must require both bazel build and bazel test:\n{prompt}",
    );
    assert!(
        prompt.contains("boss propose blocked"),
        "with the seam flag on, the gate must direct failures to the boss propose blocked \
             verb:\n{prompt}",
    );
    assert!(
        prompt.contains("FOREGROUND") && prompt.contains("run_in_background"),
        "gate must mandate foreground execution and forbid the background-and-idle anti-pattern (issue #976):\n{prompt}",
    );
}

#[test]
fn bazel_gate_present_for_chore_on_bazel_workspace_seam_off() {
    // Flag off (the builder default, matching the registry default):
    // the gate must point failures at the legacy `[blocked]` marker, not
    // `boss propose` — a worker on the flag-off path must never be
    // taught a verb the engine's read path won't yet honor.
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
        prompt.contains("## Pre-push build gate (Bazel workspace)"),
        "bazel pre-push gate must fire for code chores on a Bazel workspace:\n{prompt}",
    );
    assert!(
        !prompt.contains("boss propose"),
        "with the seam flag off, the gate must not mention boss propose at all:\n{prompt}",
    );
    assert!(
        prompt.contains("[blocked] reason=\"...\""),
        "with the seam flag off, the gate must direct failures to the legacy [blocked] \
             marker:\n{prompt}",
    );
}

#[test]
fn worker_escalation_directive_teaches_boss_propose_verbs_when_seam_is_on() {
    let ws = tempfile::TempDir::new().unwrap();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .worker_signal_proposals_seam_enabled(true)
            .build(),
    );
    assert!(
        prompt.contains("boss propose effort-escalation --level"),
        "seam on: the prompt must teach the effort-escalation verb with a worked example:\n{prompt}",
    );
    assert!(
        prompt.contains("boss propose blocked --reason"),
        "seam on: the prompt must teach the blocked verb with a worked example:\n{prompt}",
    );
    assert!(
        prompt.contains("Bootstrap fallback only:") && prompt.contains("[blocked] reason="),
        "seam on: [blocked] must be documented as the bootstrap-only fallback, not a \
             normal-path channel:\n{prompt}",
    );
    assert!(
        !prompt.contains("[effort-escalation] requested_level="),
        "seam on: new workers must no longer be taught the [effort-escalation] marker \
             grammar at all:\n{prompt}",
    );
}

#[test]
fn worker_escalation_directive_teaches_legacy_markers_when_seam_is_off() {
    // Flag off (builder default = registry default): the directive must
    // reproduce the pre-migration marker-only text byte-for-byte in
    // spirit — no `boss propose` verb anywhere, both markers taught.
    let ws = tempfile::TempDir::new().unwrap();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("boss propose"),
        "seam off: the directive must not mention boss propose at all:\n{prompt}",
    );
    assert!(
        prompt.contains("[effort-escalation] requested_level=<level> reason=\"<why>\""),
        "seam off: the directive must teach the full [effort-escalation] marker grammar:\n{prompt}",
    );
    assert!(
        prompt.contains("[blocked] reason=\"<why>\""),
        "seam off: the directive must teach the [blocked] marker grammar:\n{prompt}",
    );
}

#[test]
fn bazel_gate_absent_on_non_bazel_workspace() {
    // Empty tempdir — no MODULE.bazel / WORKSPACE marker.
    let ws = tempfile::TempDir::new().unwrap();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("Pre-push build gate"),
        "bazel gate must NOT fire on a non-Bazel repo:\n{prompt}",
    );
}

#[test]
fn bazel_gate_present_for_revision_on_bazel_workspace() {
    let ws = bazel_workspace();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/250"))
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("## Pre-push build gate (Bazel workspace)"),
        "revision chores (the #804 offenders) must get the bazel gate:\n{prompt}",
    );
}

#[test]
fn revision_prompt_omits_expected_branch_line() {
    // Issue #842: the preamble "expected branch name" line directs the
    // worker to push a fresh `boss/exec_*` bookmark, which directly
    // contradicts the revision directive's "Do NOT create a
    // `boss/exec_*` bookmark". A revision lands its commit on the
    // parent PR's existing branch, so the line must be omitted.
    let ws = tempfile::TempDir::new().unwrap();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/250"))
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("expected branch name"),
        "revision prompt must NOT template the expected-branch line (issue #842):\n{prompt}",
    );
    // The revision directive remains the only — and now uncontradicted —
    // word on branching.
    assert!(
        prompt.contains("Do NOT create a `boss/exec_*` bookmark"),
        "revision directive must still forbid creating a boss/exec_* bookmark:\n{prompt}",
    );
}

#[test]
fn chore_prompt_keeps_expected_branch_line() {
    // Guard the inverse: a fresh chore opens its own PR off a
    // `boss/exec_<id>` branch, so it must still be told the
    // engine-supplied branch name to push to.
    let ws = tempfile::TempDir::new().unwrap();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("expected branch name"),
        "a fresh chore must still receive the expected-branch line:\n{prompt}",
    );
}

#[test]
fn bazel_gate_recognizes_workspace_marker_files() {
    for marker in ["WORKSPACE", "WORKSPACE.bazel"] {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(marker), "").unwrap();
        assert!(
            is_bazel_workspace(dir.path()),
            "`{marker}` at the root must be recognized as a Bazel workspace",
        );
    }
}

// Helpers shared by revision fragment tests.

fn revision_task_with_created_via(pr_url: Option<&str>, created_via: &str) -> WorkItem {
    let mut task = crate::work::Task::builder()
        .id("task-rev-1")
        .product_id("prod-1")
        .kind(TaskKind::Revision)
        .name("Revision task")
        .description("Fix the merge conflict.")
        .status(TaskStatus::Active)
        .created_at("2026-05-15T00:00:00Z")
        .updated_at("2026-05-15T00:00:00Z")
        .autostart(false)
        .created_via(created_via)
        .build();
    task.pr_url = pr_url.map(|s| s.to_owned());
    WorkItem::Task(task)
}

fn sample_conflict_attempt() -> crate::work::ConflictResolution {
    use crate::conflict_diagnosis::{ConflictDiagnosis, ConflictedFile};
    let diag = ConflictDiagnosis {
        schema_version: 1,
        base_sha: "aaa111".into(),
        head_sha: "bbb222".into(),
        files: vec![ConflictedFile {
            path: "src/lib.rs".into(),
            marker_count: Some(1),
            shape: "content".into(),
        }],
        error: None,
    };
    crate::work::ConflictResolution {
        id: "crz_frag_01".into(),
        product_id: "prod-1".into(),
        work_item_id: "task-rev-1".into(),
        pr_url: "https://github.com/org/repo/pull/77".into(),
        pr_number: 77,
        head_branch: "feature/frag".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some("aaa111".into()),
        head_sha_before: None,
        head_sha_after: None,
        status: "running".into(),
        failure_reason: None,
        cube_lease_id: None,
        cube_workspace_id: None,
        worker_id: None,
        conflict_diagnosis: Some(serde_json::to_string(&diag).unwrap()),
        created_at: "2026-05-15T00:00:00Z".into(),
        started_at: None,
        finished_at: None,
        revision_task_id: Some("task-rev-1".into()),
        event_source: "review_watch".into(),
        conflict_class: None,
        resolved_by_rung: None,
        mechanical_rung_in_flight: None,
    }
}

fn sample_ci_attempt() -> crate::work::CiRemediation {
    crate::work::CiRemediation {
            id: "crm_frag_01".into(),
            product_id: "prod-1".into(),
            work_item_id: "task-rev-1".into(),
            pr_url: "https://github.com/org/repo/pull/77".into(),
            pr_number: 77,
            head_branch: "feature/frag".into(),
            head_sha_at_trigger: "ccc333".into(),
            head_sha_after: None,
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: r#"[{"name":"ci/test","conclusion":"FAILURE","target_url":"https://buildkite.com/myorg/mypipeline/builds/1329","provider":"buildkite","provider_job_id":"job-uuid-456"}]"#.into(),
            triage_class: None,
            log_excerpt: Some("ERROR: test failed at line 42".into()),
            status: "running".into(),
            failure_reason: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            worker_id: None,
            created_at: "2026-05-15T00:00:00Z".into(),
            started_at: None,
            finished_at: None,
            failure_kind: None,
            before_commit_sha: None,
            revision_task_id: Some("task-rev-1".into()),
        }
}

#[test]
fn revision_directive_with_conflict_provenance_injects_conflict_fragment() {
    let work_item = revision_task_with_created_via(None, "merge-conflict:crz_frag_01");
    let attempt = sample_conflict_attempt();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .conflict_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    // Must contain the conflict-resolution section header.
    assert!(
        prompt.contains("## Conflict resolution context"),
        "conflict fragment must be injected into revision directive:\n{prompt}",
    );
    // Must embed the attempt id.
    assert!(
        prompt.contains("`crz_frag_01`"),
        "conflict fragment must include the attempt id:\n{prompt}",
    );
    // Must embed the diagnosis file.
    assert!(
        prompt.contains("`src/lib.rs`"),
        "conflict fragment must render the conflict diagnosis:\n{prompt}",
    );
    // Must include the stop conditions.
    assert!(
        prompt.contains("boss engine conflicts mark-failed"),
        "conflict fragment must include the mark-failed stop condition:\n{prompt}",
    );
    // Must contain the jj first-class conflict / stacked-branch recipe.
    assert!(
        prompt.contains("jj records conflicts IN each commit independently"),
        "conflict fragment must explain jj first-class conflict model:\n{prompt}",
    );
    assert!(
        prompt.contains("Resolve from the BASE upward"),
        "conflict fragment must instruct base-up resolution for stacked branches:\n{prompt}",
    );
    assert!(
        prompt.contains("conflicts=true"),
        "conflict fragment must reference the conflicts=true log template:\n{prompt}",
    );
    assert!(
        prompt.contains("EDITOR=false"),
        "conflict fragment must warn about non-interactive editor env:\n{prompt}",
    );
    // Must still contain the base revision directive spine.
    assert!(
        prompt.contains("Do NOT create a `boss/exec_*` bookmark"),
        "base revision directive must still be present:\n{prompt}",
    );
    // P1 (incident-002): the preservation rule must be present so a
    // resolution never silently deletes functionality a merged parent
    // added.
    assert!(
        prompt.contains("Preservation rule (HARD CONSTRAINT"),
        "conflict fragment must include the preservation rule:\n{prompt}",
    );
    assert!(
        prompt.contains("must NOT remove functionality introduced by either parent"),
        "preservation rule must forbid dropping either parent's functionality:\n{prompt}",
    );
    assert!(
        prompt.contains("design-doc citation"),
        "preservation rule must require a design-doc citation for any removal:\n{prompt}",
    );
    // P4 (incident-002): the post-resolution comment must be
    // removal-forward and must not fabricate a review history.
    assert!(
        prompt.contains("⚠️ Removed"),
        "comment template must carry a prominent Removed section:\n{prompt}",
    );
    assert!(
        prompt.contains("pulls/<n>/reviews"),
        "comment guidance must condition the approvals line on the reviews API:\n{prompt}",
    );
    assert!(
        prompt.contains("OMIT that line"),
        "comment guidance must tell the worker to omit the approvals line when no review existed:\n{prompt}",
    );
}

#[test]
fn conflict_fragment_hard_gates_the_ground_truth_commands_before_local_jj_reasoning() {
    // 2026-07-23 incident (spinyfin/mono#2070): the worker went straight
    // to `jj log`, never queried `mergeable`, never ran `cube workspace
    // rebase`, and concluded "already resolved" from two
    // divergent-change-id revsets that answered about two different
    // commits. This fragment must order the two authoritative commands
    // first, forbid the local-state-only conclusion, and name the `??`
    // divergence hazard.
    let work_item = revision_task_with_created_via(None, "merge-conflict:crz_frag_01");
    let attempt = sample_conflict_attempt();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .conflict_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );

    let gh_query = prompt
        .find("--json mergeable,mergeStateStatus")
        .unwrap_or_else(|| panic!("brief must mandate the mergeable query:\n{prompt}"));
    let rebase = prompt
        .find("cube workspace rebase")
        .unwrap_or_else(|| panic!("brief must mandate the rebase:\n{prompt}"));
    assert!(
        gh_query < rebase,
        "the mergeable query must be ordered before the rebase:\n{prompt}",
    );
    // Both must precede the local-inspection recipe the worker jumped to.
    let jj_recipe = prompt
        .find("List every conflicted commit on the branch")
        .unwrap_or_else(|| panic!("brief must still carry the jj recipe:\n{prompt}"));
    assert!(
        rebase < jj_recipe,
        "the two ground-truth commands must be ordered before local jj inspection:\n{prompt}",
    );

    assert!(
        prompt.contains("You may NOT conclude \"already resolved\" from local `jj` state alone"),
        "brief must forbid concluding 'already resolved' from local jj state:\n{prompt}",
    );
    assert!(
        prompt.contains("Only `mergeable: MERGEABLE` supports a claim"),
        "brief must name MERGEABLE as the only validating value:\n{prompt}",
    );
    assert!(
        prompt.contains("`mergeable: UNKNOWN` means GitHub is still")
            && prompt.contains("not** a clean bill of health"),
        "brief must deny that UNKNOWN clears the conflict:\n{prompt}",
    );
    assert!(
        prompt.contains("`qtltpmoy??`") && prompt.contains("DIVERGENT"),
        "brief must teach the `??` divergent-change-id hazard:\n{prompt}",
    );
    assert!(
        prompt.contains("full commit ids"),
        "divergence guidance must direct the worker to full commit ids:\n{prompt}",
    );
}

#[test]
fn conflict_revision_uses_merge_correctness_gate_not_full_test_gate() {
    // A conflict-resolution revision must push the merge-corrected
    // branch as soon as it COMPILES (the merge-correctness gate); the
    // full `bazel test` suite is the PR's own CI's job, run after the
    // push. Blocking the push behind the full suite is what stranded
    // correct resolutions unpushed (the loop this fix addresses).
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
            .build(),
    );
    assert!(
        prompt.contains("## Pre-push gate for conflict resolution (Bazel workspace)"),
        "conflict revision must get the merge-correctness gate:\n{prompt}",
    );
    assert!(
        prompt.contains("Do NOT block the push on a full `bazel test //...`"),
        "conflict gate must defer the full test suite to CI:\n{prompt}",
    );
    // The generic build-AND-test-before-push gate must NOT be present
    // for conflict revisions — it is what caused the pre-push stall.
    assert!(
        !prompt.contains("## Pre-push build gate (Bazel workspace)"),
        "conflict revision must NOT carry the generic build+test gate:\n{prompt}",
    );
    assert!(
        !prompt.contains("Both `bazel build` and `bazel test` must finish clean"),
        "conflict revision must not require a full test pass before push:\n{prompt}",
    );
    // The rebase clause must reference the merge-correctness gate, not
    // the full build+test gate.
    assert!(
        prompt.contains("The full `bazel test` suite is NOT a precondition for this push"),
        "conflict rebase clause must defer tests to CI:\n{prompt}",
    );
    // Verification is NOT skipped — the merged code must still build.
    assert!(
        prompt.contains("The merged code MUST COMPILE"),
        "conflict gate must still require a clean build:\n{prompt}",
    );
}

#[test]
fn merge_order_preservation_fragment_names_merged_siblings() {
    let lines = vec!["`task_abc` (merged: https://github.com/org/repo/pull/12)".to_owned()];
    let frag = compose_merge_order_preservation_fragment(&lines);
    assert!(
        frag.contains("Merge-order preservation contract"),
        "fragment must carry the section header:\n{frag}",
    );
    assert!(
        frag.contains("task_abc"),
        "fragment must name the merged sibling:\n{frag}"
    );
    assert!(
        frag.contains("both-parents deletion tripwire"),
        "fragment must point at the tripwire as verifier:\n{frag}",
    );
}

#[test]
fn merge_order_preservation_fragment_is_empty_without_merged_siblings() {
    assert!(
        compose_merge_order_preservation_fragment(&[]).is_empty(),
        "no merged overlap partner ⇒ no sibling-specific clause",
    );
}

#[test]
fn render_merge_order_lines_include_pr_url_when_present() {
    let siblings = vec![
        crate::work_dependencies::MergeOrderMergedSibling {
            task_id: "task_with_pr".into(),
            pr_url: Some("https://github.com/org/repo/pull/9".into()),
        },
        crate::work_dependencies::MergeOrderMergedSibling {
            task_id: "task_no_pr".into(),
            pr_url: None,
        },
    ];
    let lines = render_merge_order_preservation_lines(&siblings);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("task_with_pr") && lines[0].contains("pull/9"));
    assert!(lines[1].contains("task_no_pr") && !lines[1].contains("merged:"));
}

#[test]
fn revision_directive_injects_merge_order_preservation_when_sibling_merged() {
    let work_item = revision_task_with_created_via(None, "merge-conflict:crz_frag_01");
    let attempt = sample_conflict_attempt();
    let lines = vec!["`task_sibling` (merged: https://github.com/org/repo/pull/50)".to_owned()];
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .conflict_attempt(&attempt)
            .merge_order_preservation(&lines)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("Merge-order preservation contract"),
        "conflict brief must carry the sibling-specific preservation clause:\n{prompt}",
    );
    assert!(
        prompt.contains("task_sibling"),
        "clause must name the merged sibling:\n{prompt}",
    );
}

#[test]
fn revision_directive_omits_merge_order_clause_without_merged_sibling() {
    // A conflict revision with no merged overlap partner keeps only the
    // generic preservation rule — no sibling-specific block.
    let work_item = revision_task_with_created_via(None, "merge-conflict:crz_frag_01");
    let attempt = sample_conflict_attempt();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .conflict_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("Merge-order preservation contract"),
        "no merged sibling ⇒ no sibling-specific block:\n{prompt}",
    );
    // The generic preservation rule is still present.
    assert!(prompt.contains("Preservation rule (HARD CONSTRAINT"));
}

#[test]
fn non_conflict_revision_keeps_full_build_and_test_gate() {
    // A plain operator revision (no conflict attempt) keeps the
    // build-AND-test-before-push gate — the merge-correctness rescope
    // is conflict-resolution-specific.
    let ws = bazel_workspace();
    let work_item = revision_task_with_created_via(None, "operator");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("## Pre-push build gate (Bazel workspace)"),
        "non-conflict revision must keep the generic build+test gate:\n{prompt}",
    );
    assert!(
        prompt.contains("Both `bazel build` and `bazel test` must finish clean"),
        "non-conflict revision must still require a full test pass before push:\n{prompt}",
    );
    assert!(
        !prompt.contains("## Pre-push gate for conflict resolution"),
        "non-conflict revision must NOT get the conflict merge-correctness gate:\n{prompt}",
    );
}

#[test]
fn revision_directive_with_ci_fix_provenance_injects_ci_fragment() {
    let work_item = revision_task_with_created_via(None, "ci-fix:crm_frag_01");
    let attempt = sample_ci_attempt();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .ci_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    // Must contain the CI remediation section header.
    assert!(
        prompt.contains("## CI remediation context"),
        "CI fragment must be injected into revision directive:\n{prompt}",
    );
    // Must embed the attempt id.
    assert!(
        prompt.contains("`crm_frag_01`"),
        "CI fragment must include the attempt id:\n{prompt}",
    );
    // Must embed the failing check name.
    assert!(
        prompt.contains("`ci/test`"),
        "CI fragment must render the failing check list:\n{prompt}",
    );
    // Must embed the log excerpt.
    assert!(
        prompt.contains("ERROR: test failed at line 42"),
        "CI fragment must include the log excerpt:\n{prompt}",
    );
    // Must contain the pre-filled bk commands block.
    assert!(
        prompt.contains("bk build view 1329 --pipeline mypipeline"),
        "CI fragment must include pre-filled bk build view command:\n{prompt}",
    );
    assert!(
        prompt.contains("bk job log --pipeline mypipeline --build-number 1329 job-uuid-456"),
        "CI fragment must include pre-filled bk job log command:\n{prompt}",
    );
    // Must still contain the base revision directive spine.
    assert!(
        prompt.contains("Do NOT create a `boss/exec_*` bookmark"),
        "base revision directive must still be present:\n{prompt}",
    );
}

/// A bespoke `ci_remediation`-kind execution routes to
/// `compose_ci_remediation_prompt` (the retrigger playbook) instead
/// of the revision directive.
fn ci_remediation_execution(pr_url: &str) -> WorkExecution {
    WorkExecution::builder()
        .id("exec_cir_01")
        .work_item_id("task-cir-1")
        .kind(ExecutionKind::CiRemediation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@github.com:org/repo.git")
        .workspace_path("/tmp/workspace")
        .pr_url(pr_url)
        .created_at("2026-05-15T00:00:00Z")
        .build()
}

#[test]
fn ci_remediation_prompt_offers_mark_noop_for_non_rebounce() {
    // A bespoke (retrigger / stranded-rescue) ci_remediation worker
    // should be told it can declare a validated noop if the failure
    // already cleared — the engine re-probes live CI before honoring
    // it. `sample_ci_attempt` has `failure_kind: None`.
    let attempt = sample_ci_attempt();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&ci_remediation_execution(&attempt.pr_url))
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .ci_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("### If CI is already green (nothing to fix)"),
        "non-rebounce ci_remediation prompt must offer the validated mark-noop escape:\n{prompt}",
    );
    assert!(
        prompt.contains("boss engine ci mark-noop --attempt-id"),
        "non-rebounce ci_remediation prompt must include the mark-noop verb:\n{prompt}",
    );
}

#[test]
fn ci_remediation_prompt_omits_mark_noop_for_rebounce() {
    // A merge_queue_rebounce failure lives on the synthetic merge
    // commit, so the PR's head-branch checks always read green. The
    // engine REJECTS a rebounce noop outright
    // (`handle_mark_ci_remediation_noop`), so the brief must NOT
    // surface it — mirroring the `!is_rebounce` gate in the sibling
    // `compose_ci_remediation_fragment`. (A rebounce normally
    // delivers via a revision; the stranded-rescue path can still
    // re-dispatch one through this bespoke prompt.)
    let mut attempt = sample_ci_attempt();
    attempt.failure_kind = Some("merge_queue_rebounce".into());
    attempt.before_commit_sha = Some("mergesha999".into());
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&ci_remediation_execution(&attempt.pr_url))
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .ci_attempt(&attempt)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("mark-noop"),
        "rebounce ci_remediation prompt must NOT surface mark-noop (engine rejects it):\n{prompt}",
    );
    // Sanity: we still produced the bespoke ci_remediation prompt.
    assert!(
        prompt.contains("CI remediation: PR #"),
        "expected the bespoke ci_remediation prompt to be generated:\n{prompt}",
    );
}

#[test]
fn revision_directive_without_provenance_has_no_fragment() {
    // Operator-triggered revision: no conflict or CI attempt → no fragment.
    let work_item = revision_task_with_created_via(None, "operator");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("## Conflict resolution context"),
        "no conflict fragment for operator revision:\n{prompt}",
    );
    assert!(
        !prompt.contains("## CI remediation context"),
        "no CI fragment for operator revision:\n{prompt}",
    );
    assert!(
        prompt.contains("Do NOT create a `boss/exec_*` bookmark"),
        "base revision directive must still be present:\n{prompt}",
    );
}

#[test]
fn revision_directive_requires_pr_title_update() {
    // Issue #843 (motivating incident T132 / PR #713): the revision worker
    // correctly fixed the code but left the original PR title and body
    // arguing the now-overturned conclusion. The directive must instruct
    // the worker to update BOTH the title and the description to match the
    // final state — not just the description.
    let work_item = revision_task_with_created_via(None, "operator");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("Update the PR title AND description"),
        "revision directive must instruct updating BOTH title and description:\n{prompt}",
    );
    assert!(
        prompt.contains("title MUST be updated") || prompt.contains("title MUST reflect"),
        "revision directive must have a hard instruction to update the title when scope/conclusion changes:\n{prompt}",
    );
    assert!(
        prompt.contains("--title"),
        "revision directive must show the gh pr edit --title command:\n{prompt}",
    );
    assert!(
        prompt.contains("gh pr edit 77 --title"),
        "revision directive must interpolate the actual PR number into the title command, not emit literal {{pr_number}}:\n{prompt}",
    );
    assert!(
        !prompt.contains("{pr_number}"),
        "revision directive must not emit the literal placeholder {{pr_number}} — it must be interpolated:\n{prompt}",
    );
}

// -----------------------------------------------------------------------
// `[deferred-scope]` marker directive (Flunge T254, root-caused to
// T222/PR #765)
// -----------------------------------------------------------------------

#[test]
fn deferred_scope_directive_present_for_chore_implementation() {
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("[deferred-scope] summary=\"<what you did not deliver>\""),
        "chore_implementation prompt must teach the [deferred-scope] marker grammar:\n{prompt}",
    );
    assert!(
        prompt.contains("filed as a followup"),
        "directive must forbid the false \"filed as a followup\" claim:\n{prompt}",
    );
    assert!(
        prompt.contains("only sanctioned channel for declaring deferred scope"),
        "directive must state the marker is the only sanctioned channel — prose-only \
             deferral declarations (e.g. a \"## Deferred\" section with no markers) must be \
             called out as insufficient:\n{prompt}",
    );
    assert!(
        prompt.contains("protocol violation"),
        "directive must state that a prose deferral section with no matching markers is a \
             protocol violation reviewers are instructed to flag:\n{prompt}",
    );
}

#[test]
fn deferred_scope_directive_present_for_revision_implementation() {
    let work_item = revision_task_with_created_via(None, "operator");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&revision_execution("https://github.com/org/repo/pull/77"))
            .work_item(&work_item)
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("[deferred-scope] summary=\"<what you did not deliver>\""),
        "revision_implementation prompt must teach the [deferred-scope] marker grammar:\n{prompt}",
    );
}

#[test]
fn deferred_scope_directive_absent_for_answer_agent() {
    // The answer agent is read-only and never delivers scope against a
    // brief — it must not be taught a marker it has no reason to emit.
    let execution = WorkExecution::builder()
        .id("exec_answer_01")
        .work_item_id("task-1")
        .kind(ExecutionKind::AnswerAgent)
        .status(ExecutionStatus::Running)
        .repo_remote_url("git@github.com:org/repo.git")
        .workspace_path("/tmp/workspace")
        .created_at("2026-05-15T00:00:00Z")
        .build();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&execution)
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        !prompt.contains("[deferred-scope]"),
        "read-only answer-agent prompt must not carry the [deferred-scope] directive:\n{prompt}",
    );
}

// -----------------------------------------------------------------------
// editorial-rules block rendering (chore #5)
// -----------------------------------------------------------------------

#[test]
fn editorial_rules_block_always_rendered_with_baked_in_rules() {
    // Default config: block always appears with baked-in rules.
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .editorial_enabled(true)
            .build(),
    );
    assert!(
        prompt.contains("[editorial-rules]"),
        "editorial-rules block must always be present:\n{prompt}",
    );
    assert!(
        prompt.contains("[/editorial-rules]"),
        "editorial-rules closing tag must be present:\n{prompt}",
    );
    assert!(
        prompt.contains("exec_\u{2026}"),
        "baked-in identifier rule must be present:\n{prompt}",
    );
    assert!(
        prompt.contains("Boss worker"),
        "baked-in phrase rule must be present:\n{prompt}",
    );
}

#[test]
fn editorial_rules_block_default_config_has_no_instructions_section() {
    // Default config: no instructions, no template, no enforcement banner.
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .editorial_enabled(true)
            .build(),
    );
    assert!(
        !prompt.contains("Product-specific rules"),
        "default config must not render instructions section:\n{prompt}",
    );
    assert!(
        !prompt.contains("Template policy"),
        "default config must not render template section:\n{prompt}",
    );
    assert!(
        !prompt.contains("Enforcement:"),
        "default config must not render enforcement banner:\n{prompt}",
    );
}

#[test]
fn editorial_rules_block_with_instructions_renders_full_configured_sections() {
    let rules = boss_protocol::EditorialRules {
        instructions: Some("No emoji in PR titles.".to_owned()),
        ..Default::default()
    };
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .editorial_rules(&rules)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .editorial_enabled(true)
            .build(),
    );
    assert!(
        prompt.contains("Product-specific rules"),
        "configured product must render instructions section:\n{prompt}",
    );
    assert!(
        prompt.contains("No emoji in PR titles."),
        "configured product must include verbatim instructions:\n{prompt}",
    );
    assert!(
        prompt.contains("Enforcement:"),
        "configured product must render enforcement banner:\n{prompt}",
    );
}

#[test]
fn editorial_rules_block_with_enforce_template_includes_template_text() {
    let tmpl = crate::pr_template::PrTemplate {
        text: "## Summary\n\n## Test plan\n".to_owned(),
        required_headings: vec!["Summary".to_owned(), "Test plan".to_owned()],
        source_path: std::path::PathBuf::from(".github/PULL_REQUEST_TEMPLATE.md"),
    };
    let pr_template_set = crate::pr_template::PrTemplateSet {
        default_template: Some(tmpl),
        named_templates: std::collections::HashMap::new(),
    };
    let rules = boss_protocol::EditorialRules {
        template_policy: boss_protocol::TemplatePolicy::Enforce,
        ..Default::default()
    };
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .editorial_rules(&rules)
            .pr_template_set(&pr_template_set)
            .editorial_enabled(true)
            .build(),
    );
    assert!(
        prompt.contains("Template policy: Enforce"),
        "enforce policy must appear in prompt:\n{prompt}",
    );
    assert!(
        prompt.contains("## Summary"),
        "template content must be rendered verbatim:\n{prompt}",
    );
    assert!(
        prompt.contains("## Test plan"),
        "template content must be rendered verbatim:\n{prompt}",
    );
    assert!(
        prompt.contains("Enforcement:"),
        "enforcement banner must be present for configured product:\n{prompt}",
    );
}

#[test]
fn editorial_rules_block_appears_before_per_kind_directive() {
    // [editorial-rules] must appear before "Expected outcome for this run:"
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .editorial_enabled(true)
            .build(),
    );
    let editorial_pos = prompt.find("[editorial-rules]").expect("editorial-rules block missing");
    let directive_pos = prompt
        .find("Expected outcome for this run:")
        .expect("per-kind directive missing");
    assert!(
        editorial_pos < directive_pos,
        "editorial-rules block must appear before the per-kind directive:\n{prompt}",
    );
}

#[test]
fn editorial_rules_block_advise_template_policy_rendered() {
    let rules = boss_protocol::EditorialRules {
        template_policy: boss_protocol::TemplatePolicy::Advise,
        ..Default::default()
    };
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .editorial_rules(&rules)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .editorial_enabled(true)
            .build(),
    );
    assert!(
        prompt.contains("Template policy: Advise"),
        "advise policy must appear in prompt:\n{prompt}",
    );
    assert!(
        prompt.contains("Enforcement:"),
        "enforcement banner must be present when template policy is set:\n{prompt}",
    );
}

// -----------------------------------------------------------------------
// editorial_controls feature flag (kill switch, default off)
// -----------------------------------------------------------------------

#[test]
fn editorial_controls_flag_off_omits_block() {
    // With editorial_enabled = false, no [editorial-rules] block in the prompt.
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .editorial_enabled(false)
            .build(),
    );
    assert!(
        !prompt.contains("[editorial-rules]"),
        "editorial-rules block must be absent when flag is off:\n{prompt}",
    );
    assert!(
        !prompt.contains("[/editorial-rules]"),
        "editorial-rules closing tag must be absent when flag is off:\n{prompt}",
    );
    // Prompt must still be a valid worker prompt (has execution context).
    assert!(
        prompt.contains("execution id"),
        "prompt must still contain execution context when editorial is off:\n{prompt}",
    );
}

#[test]
fn editorial_controls_flag_off_omits_block_even_with_configured_rules() {
    // Rules configured on the product are also suppressed when the flag is off.
    let rules = boss_protocol::EditorialRules {
        instructions: Some("No emoji in titles.".to_owned()),
        ..Default::default()
    };
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .editorial_rules(&rules)
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .editorial_enabled(false)
            .build(),
    );
    assert!(
        !prompt.contains("[editorial-rules]"),
        "editorial-rules block must be absent when flag is off (even with rules configured):\n{prompt}",
    );
}

#[test]
fn editorial_controls_flag_on_preserves_existing_behavior() {
    // With editorial_enabled = true, the [editorial-rules] block must be present
    // and contain the baked-in rules — identical to the original behavior.
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&base_execution())
            .work_item(&chore_without_pr())
            .workspace_path(std::path::Path::new("/tmp/workspace"))
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .editorial_enabled(true)
            .build(),
    );
    assert!(
        prompt.contains("[editorial-rules]"),
        "editorial-rules block must be present when flag is on:\n{prompt}",
    );
    assert!(
        prompt.contains("[/editorial-rules]"),
        "editorial-rules closing tag must be present when flag is on:\n{prompt}",
    );
    assert!(
        prompt.contains("exec_\u{2026}"),
        "baked-in identifier rule must be present when flag is on:\n{prompt}",
    );
}

/// The design directive must require the `Scope:` tag on every breakdown
/// entry — this is the structured marker the Planner's system prompt
/// (`tools/boss/engine/core/src/planner.rs`) keys off to park deferred
/// items instead of proposing them as ordinary startable work.
#[test]
fn design_directive_requires_the_scope_tag() {
    let directive = compose_design_directive(None);
    assert!(directive.contains("Scope: in-scope"), "{directive}");
    assert!(
        directive.contains("Scope: deferred (future / not a v1 blocker)"),
        "{directive}"
    );
    assert!(
        directive.contains("downstream scheduling keys off it verbatim"),
        "{directive}"
    );
    assert!(directive.contains("rather than silently omitting them"), "{directive}");
}
