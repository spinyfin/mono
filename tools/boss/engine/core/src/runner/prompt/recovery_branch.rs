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

/// Render the `## STARTUP RECOVERY` block for a worker respawned after its
/// predecessor was interrupted.
///
/// ## Why this only fires on a durable pointer
///
/// The engine's operating rule for recovery is that it fires only on an
/// unambiguous durable pointer the system itself wrote — restart fresh on
/// doubt. A branch-resume instruction is therefore emitted only once the
/// predecessor's expected remote ref (computed from *its own* frozen
/// `branch_naming`/`worker_branch_prefix`, not the successor's) has been
/// verified against GitHub: a confirmed ref gets a `jj edit ...@origin`
/// instruction, a confirmed 404 absence explicitly tells the worker not to
/// run one, and any other probe failure is inconclusive and renders
/// neither claim.
///
/// The only thing the engine *does* durably record is [recovered workspace
/// state](boss_engine_recovery::recovery_apply): a marker
/// (`.boss/recovery-report.json`) it writes itself when it actually recovers
/// something, in place or from a saved patch. This function is now called
/// only when that marker exists for this execution — see
/// [`super::compose_execution_prompt`]. When it doesn't, [`super::compose_execution_prompt`]
/// renders no block at all: the ordinary "expected branch name" / `jj new
/// main` guidance already in the prompt is the correct, honest instruction
/// for a fresh start, and no extra text is needed to say so.
///
/// ## What the block says
///
/// It describes recovered state, tells the worker to inspect it before
/// building on it, and includes a prior branch only when the remote confirms
/// that branch exists.
///
/// A `patch_error` on the report means recovery FAILED. That case gets its
/// own paragraph telling the worker not to assume anything was resumed —
/// silence there would leave it guessing, which is how a "recovered" worker
/// quietly redoes everything or, worse, half-redoes it.
pub(super) fn startup_recovery_block(report: &boss_engine_recovery::recovery_apply::RecoveryReport) -> String {
    use boss_engine_recovery::recovery_apply::RecoverySource;

    let mut block = String::from("## STARTUP RECOVERY\n\n");
    if report.from_execution_id.is_empty() {
        block.push_str(
            "This execution was respawned after the previous worker session was interrupted \
             (engine or UI crash). The engine recovered its state into this workspace — treat \
             what follows as a recovered mid-thought, not as a reviewed starting point.\n\n",
        );
    } else {
        block.push_str(&format!(
            "This execution was respawned after execution `{}` was interrupted (engine or UI \
             crash). The engine recovered its state into this workspace — treat what follows as \
             a recovered mid-thought, not as a reviewed starting point.\n\n",
            report.from_execution_id,
        ));
    }

    if let Some(err) = report.patch_error.as_deref() {
        block.push_str(&format!(
            "### Recovery FAILED\n\
             \n\
             The engine had a saved patch of the prior worker's uncommitted work but it \
             did NOT apply:\n\
             \n\
             ```\n{err}\n```\n\
             \n\
             **Do NOT assume any of the prior work is present.** Your working copy holds \
             whatever the workspace already had — most likely nothing. Verify with \
             `jj status` and `jj diff --stat` before you plan, and expect to redo the \
             prior work from the task description. The patch was deliberately left on \
             disk so a human can salvage it; say so in your summary if the redo is \
             substantial.\n\n",
        ));
    } else if report.source == RecoverySource::CubeInPlace {
        block.push_str(
            "### State recovered IN PLACE\n\
             \n\
             You are running in the *same* cube workspace the interrupted worker was \
             using, and its uncommitted working copy is intact — including its jj \
             operation log. **Do not reset it.** Start by looking at what is already \
             there:\n\
             \n\
             ```\n\
             jj status\n\
             jj diff --stat\n\
             jj log -r '::@' -n 10\n\
             ```\n\
             \n\
             Read the recovered changes before adding to them. They are a crashed \
             worker's in-progress edits: they may be half-finished, may not compile, and \
             may not match the current task description. Reconcile them against the \
             brief first, then continue.\n\n",
        );
    } else {
        // RecoverySource::Patch, applied successfully.
        let summary = report
            .applied
            .as_ref()
            .map(|a| a.summary())
            .unwrap_or_else(|| "nothing".to_string());
        let files = report
            .applied
            .as_ref()
            .map(|a| a.paths.iter().map(|p| format!("  - `{p}`\n")).collect::<String>())
            .unwrap_or_default();
        block.push_str(&format!(
            "### State recovered FROM A PATCH\n\
             \n\
             The interrupted worker's cube workspace could not be reclaimed, so the \
             engine replayed its saved patch into THIS workspace. Restored: \
             {summary}.\n\
             \n\
             Files restored:\n{files}\
             \n\
             These are **uncommitted edits only** — the prior worker's jj history and \
             operation log did not come with them, and Boss's own bookkeeping files were \
             filtered out. **Do not reset the working copy.** Inspect before building on \
             it:\n\
             \n\
             ```\n\
             jj status\n\
             jj diff --stat\n\
             ```\n\
             \n\
             A three-way apply can leave edits that do not compile or that reference \
             things that have since changed on `main`. Verify the restored state builds \
             and matches the task description before adding to it.\n\n",
        ));
    }

    block
}
