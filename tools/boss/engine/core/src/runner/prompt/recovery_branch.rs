//! Remote-verified prior-branch guidance for recovery prompts.

/// Result of probing the predecessor's own frozen branch-naming settings
/// against the remote: whether the branch it names actually exists there,
/// and the branch name itself — computed from the PREDECESSOR execution's
/// `branch_naming`/`worker_branch_prefix`, never the successor's. Carrying
/// the verified string alongside the bool keeps the renderer from having to
/// (mis-)recompute it from the wrong execution's settings.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::runner) struct PriorBranchProbe {
    pub(in crate::runner) exists: bool,
    pub(in crate::runner) branch: String,
}

/// Render branch-resume guidance only after worker spawn checked the remote
/// ref. A missing ref is useful information too: make the absence explicit
/// without handing the worker a command guaranteed to fail.
pub(super) fn prior_branch_block(
    report: &boss_engine_recovery::recovery_apply::RecoveryReport,
    prior_branch: Option<&PriorBranchProbe>,
) -> Option<String> {
    match (report.from_execution_id.is_empty(), prior_branch) {
        (false, Some(probe)) if probe.exists => {
            let branch = &probe.branch;
            Some(format!(
                "### Prior pushed branch\n\n\
                 The prior worker also pushed branch `{branch}`. Inspect the recovered working \
                 state above first; if you need its committed history, resume it with:\n\n\
                 ```\n\
                 jj edit {branch}@origin\n\
                 ```\n\n",
            ))
        }
        (false, Some(probe)) if !probe.exists => Some(
            "### Prior pushed branch\n\n\
             The engine checked the prior worker's expected branch and it was not pushed to \
             the remote. Do not run a branch-resume command: the recovered workspace state \
             above is the only available handoff.\n\n"
                .to_owned(),
        ),
        _ => None,
    }
}
