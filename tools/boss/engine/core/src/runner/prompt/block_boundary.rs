//! Opening autonomy / block-boundary for implementation-family worker prompts.
//!
//! Split out of `prompt.rs` to keep that file under the repo's `file/size`
//! check. The one-line "avoid asking; stop and summarize" instruction lived
//! here and workers were reading it as license to park on merge conflicts or
//! CI failures they could handle. This fragment keeps the genuine-decision
//! stop path and names the cases that are not blockers.

/// Opening block-boundary text injected at the top of
/// [`super::compose_execution_prompt`].
///
/// Keep the wording specific: a blanket "never block", "always resolve
/// conflicts yourself", or "always fix CI yourself" would swing past the
/// failure this exists to fix.
pub(super) fn block_boundary_fragment() -> &'static str {
    "Avoid asking the human for permission during this pass.\n\
     \n\
     Stop and declare blocked only when you need a decision you have no \
     authority or information to make:\n\
     - Two defensible resolutions exist and the choice changes product \
     behaviour, with nothing in the brief, the design doc, or the code to \
     disambiguate.\n\
     - A required credential, permission, or external system is genuinely \
     unavailable.\n\
     - The brief conflicts with what the code actually does, in a way that \
     means delivering it as written would be wrong.\n\
     When one of those applies, stop and summarize it clearly.\n\
     \n\
     These are not reasons to block — they are work you are capable of doing:\n\
     - Merge conflicts in your own branch, including rebasing onto a moved \
     `main` and resolving the result. Rebase, resolve, verify, and carry on.\n\
     - A CI failure caused by your change. Fix it; there is no judgement \
     call.\n\
     - Work that is tedious, large, or feels risky but is within the brief.\n\
     \n\
     When a merge conflict itself requires a product decision — most often a \
     semantic conflict where both sides changed the same behaviour and the \
     correct combination is not inferable — you may still escalate. Then \
     state the specific decision you need, naming the files and the two \
     candidate resolutions. \"There are conflicts\" is not an escalation; \
     \"both #1592 and #1593 changed how the composition sheet orders its \
     sections and I cannot tell which order is wanted\" is.\n\
     \n\
     Resolve conflicts by integrating both sides. Do not discard one side \
     wholesale, take `--ours`/`--theirs`, or revert what landed on `main`.\n\
     \n\
     Classify a CI failure by cause, then act — do not park, and do not \
     leave \"someone else will handle it\" as an available excuse:\n\
     - Caused by your change: fix it. No judgement call, not grounds to \
     block, no handoff.\n\
     - Unrelated to your change AND trivial: fix it inline — that is what \
     a human would do.\n\
     - Unrelated AND not trivial: do not absorb it into this PR. Do not \
     silently expand this PR into repairing someone else's breakage — \
     that makes the diff unreviewable and couples two unrelated changes. \
     This is the case where stopping to escalate can be legitimate. Flag \
     it, stating whether the remedy is a separate fix or reverting the \
     offending commit, and why. \"CI is red\" is not an escalation; name \
     the specific problem and the proposed remedy.\n\
     \n\
     Whichever branch you take, the PR description must say which \
     category you judged the failure into and on what basis. A silent \
     inline fix of an unrelated failure is nearly as bad as ignoring it, \
     because nobody can review a decision they cannot see.\n\
     \n\
     Trivial means the correct fix is mechanical and obviously correct — \
     the canonical case is `main` adding a required `is_review_supervisor` \
     field to `StartWorkerInput` after a test was written, breaking clippy \
     on an unrelated PR's merge-queue synthetic commit. One line, \
     mechanical. A failing test whose correct behaviour you would have to \
     reason about, or a failure whose cause you cannot identify, is not \
     trivial, regardless of how few lines the fix looks.\n\
     \n\
     The engine already mints CI-fix revisions. That path is the fallback \
     for failures that genuinely need a separate change (the \
     unrelated-and-not-trivial case). It is not a reason to punt a \
     failure you could fix in this run.\n\
     \n\
     Do not disable, skip, `#[ignore]`, or delete a failing test; do not \
     add a file to a check exclusion or allowlist, or add a lint-disable \
     comment; do not mark a failure flaky and retry it without evidence; \
     do not weaken assertions or raise thresholds; do not use \
     `--no-verify` or otherwise bypass a gate.\n\n"
}

#[cfg(test)]
mod tests {
    use super::super::{ExecutionPromptParams, compose_execution_prompt};
    use super::*;
    use crate::work::{Task, WorkExecution, WorkItem};
    use boss_protocol::{ExecutionKind, ExecutionStatus, TaskKind, TaskStatus};

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

    fn compose(work_item: &WorkItem) -> String {
        compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(work_item)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        )
    }

    /// Property pins for the fragment itself, matching the prompt-text
    /// test pattern PR #2889 established on the large-effort addendum:
    /// assert the required teaching, and assert the overcorrections are
    /// absent, rather than snapshotting the whole string.
    #[test]
    fn fragment_says_resolvable_merge_conflicts_are_not_blockers() {
        let frag = block_boundary_fragment();
        assert!(
            frag.contains("Avoid asking the human for permission during this pass."),
            "must keep the existing no-permission-ask instruction; got: {frag}",
        );
        assert!(
            frag.contains("Merge conflicts in your own branch"),
            "must say merge conflicts in the worker's own branch are not blockers; got: {frag}",
        );
        assert!(
            frag.contains("Rebase, resolve, verify, and carry on"),
            "must tell the worker to rebase/resolve/verify rather than park; got: {frag}",
        );
        assert!(
            frag.contains("\"There are conflicts\" is not an escalation"),
            "must reject a bare 'there are conflicts' escalation; got: {frag}",
        );
        assert!(
            frag.contains("naming the files and the two candidate resolutions"),
            "a genuine conflict-decision escalation must name files and candidates; got: {frag}",
        );
        assert!(
            frag.contains("stop and summarize it clearly"),
            "genuine blockers must still stop and summarize; got: {frag}",
        );
    }

    #[test]
    fn fragment_classifies_ci_failures_instead_of_treating_them_as_blockers() {
        let frag = block_boundary_fragment();
        assert!(
            frag.contains("A CI failure caused by your change"),
            "a CI failure caused by the worker's change must not be a blocker; got: {frag}",
        );
        assert!(
            frag.contains("Caused by your change: fix it"),
            "caused-by-you CI failures must be fixed with no judgement call; got: {frag}",
        );
        assert!(
            frag.contains("Unrelated to your change AND trivial"),
            "unrelated-and-trivial CI failures must be fixed inline; got: {frag}",
        );
        assert!(
            frag.contains("which category you judged the failure into"),
            "every CI-failure branch must state the category in the PR, not only the trivial one; got: {frag}",
        );
        assert!(
            frag.contains("A silent inline fix of an unrelated failure is nearly as bad as ignoring it"),
            "a silent unrelated fix must be called out as unreviewable; got: {frag}",
        );
        assert!(
            frag.contains("Unrelated AND not trivial") && frag.contains("do not absorb it into this PR"),
            "unrelated-and-not-trivial CI failures must be flagged, not absorbed; got: {frag}",
        );
        assert!(
            frag.contains("\"CI is red\" is not an escalation"),
            "a bare 'CI is red' report must not count as an escalation; got: {frag}",
        );
        assert!(
            frag.contains("`is_review_supervisor`")
                && frag.contains("`StartWorkerInput`")
                && frag.contains("whose cause you cannot identify, is not trivial"),
            "must calibrate trivial vs not with the StartWorkerInput case vs unidentified/behavioural; got: {frag}",
        );
        assert!(
            frag.contains("fallback for failures that genuinely need a separate change")
                && frag.contains("not a reason to punt"),
            "CI-fix revisions are the separate-change fallback, not a punt; got: {frag}",
        );
        assert!(
            frag.contains("or delete a failing test")
                && frag.contains("check exclusion or allowlist")
                && frag.contains("`--no-verify`"),
            "must forbid skip/ignore/delete/exclusion/flaky-without-evidence/--no-verify; got: {frag}",
        );
    }

    #[test]
    fn fragment_does_not_overcorrect_to_never_block() {
        let frag = block_boundary_fragment();
        let lower = frag.to_ascii_lowercase();
        assert!(
            !lower.contains("never block"),
            "must not swing to a blanket never-block instruction; got: {frag}",
        );
        assert!(
            !lower.contains("always resolve conflicts yourself"),
            "must not drop the genuine-decision carve-out; got: {frag}",
        );
        assert!(
            !lower.contains("always fix ci"),
            "must not drop the unrelated-and-not-trivial CI carve-out; got: {frag}",
        );
        assert!(
            frag.contains("Stop and declare blocked only when"),
            "must keep the blocked path for genuine missing decisions; got: {frag}",
        );
        assert!(
            frag.contains("you may still escalate"),
            "a semantic conflict that needs a product decision must still be escalatable; got: {frag}",
        );
        assert!(
            frag.contains("Do not discard one side"),
            "must not teach wholesale --ours/--theirs as the resolution; got: {frag}",
        );
    }

    #[test]
    fn assembled_worker_prompt_contains_block_boundary() {
        let frag = block_boundary_fragment();
        for work_item in [chore_without_pr(), chore_with_pr("https://github.com/org/repo/pull/42")] {
            let prompt = compose(&work_item);
            assert!(
                prompt.contains(frag),
                "assembled worker prompt must embed the block-boundary fragment:\n{prompt}",
            );
            assert!(
                prompt.contains("classify it per the CI-failure rules at the top of this prompt"),
                "CI-monitoring failed-check sentence must point at the opening classification:\n{prompt}",
            );
            assert!(
                prompt.contains("[blocked] reason="),
                "assembled prompt must still teach the blocked marker:\n{prompt}",
            );
            assert!(
                !prompt.contains("conflict you cannot resolve"),
                "resume/stop text must not treat a generic unresolvable conflict as a blocker:\n{prompt}",
            );
        }
        let resumed = compose(&chore_with_pr("https://github.com/org/repo/pull/42"));
        assert!(
            resumed.contains("A merge conflict on that branch is not a resume failure"),
            "resume path must not reintroduce conflicts-as-blockers:\n{resumed}",
        );
    }
}
