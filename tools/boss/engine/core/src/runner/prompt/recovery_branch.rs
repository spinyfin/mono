//! Remote-verified prior-branch guidance for recovery prompts.

use crate::work::WorkExecution;

/// Render branch-resume guidance only after worker spawn checked the remote
/// ref. A missing ref is useful information too: make the absence explicit
/// without handing the worker a command guaranteed to fail.
pub(super) fn prior_branch_block(
    report: &boss_engine_recovery::recovery_apply::RecoveryReport,
    execution: &WorkExecution,
    prior_branch_exists: Option<bool>,
) -> Option<String> {
    match (report.from_execution_id.is_empty(), prior_branch_exists) {
        (false, Some(true)) => {
            let branch = crate::completion::expected_branch_name(
                &report.from_execution_id,
                &execution.branch_naming,
                execution.worker_branch_prefix.as_deref(),
            );
            Some(format!(
                "### Prior pushed branch\n\n\
                 The prior worker also pushed branch `{branch}`. Inspect the recovered working \
                 state above first; if you need its committed history, resume it with:\n\n\
                 ```\n\
                 jj edit {branch}@origin\n\
                 ```\n\n",
            ))
        }
        (false, Some(false)) => Some(
            "### Prior pushed branch\n\n\
             The engine checked the prior worker's expected branch and it was not pushed to \
             the remote. Do not run a branch-resume command: the recovered workspace state \
             above is the only available handoff.\n\n"
                .to_owned(),
        ),
        _ => None,
    }
}
