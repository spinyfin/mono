//! Worker prompt composition: [`compose_execution_prompt`] and the directive /
//! fragment builder family (Bazel gates, editorial rules, escalation /
//! deferred-scope / no-op / CI-monitoring directives, and the design /
//! investigation / revision / conflict / CI-remediation fragments).

use std::path::Path;

use crate::ci_log_reader::{parse_buildkite_build_id, parse_buildkite_pipeline_slug};
use crate::conflict_diagnosis::ConflictDiagnosis;
use crate::work::{
    CiRemediation, ConflictResolution, Project, WorkDb, WorkExecution, WorkItem, parse_pr_doc_artifact_id,
};
use boss_protocol::{EditorialRules, ExecutionKind, TaskKind, TemplatePolicy};

use super::work_item::{project_details, work_item_details, work_item_name, work_item_pr_url};

#[derive(bon::Builder)]
pub(super) struct ExecutionPromptParams<'a> {
    execution: &'a WorkExecution,
    work_item: &'a WorkItem,
    workspace_path: &'a Path,
    parent_project: Option<&'a Project>,
    cube_change_id: Option<&'a str>,
    conflict_attempt: Option<&'a ConflictResolution>,
    recovery_branch: Option<&'a str>,
    ci_attempt: Option<&'a CiRemediation>,
    editorial_rules: Option<&'a EditorialRules>,
    pr_template_set: &'a crate::pr_template::PrTemplateSet,
    #[builder(default)]
    editorial_enabled: bool,
    /// Whether `worker_signal_proposals_seam` is on — gates the worker-facing
    /// prompt half of this seam
    /// (`worker-proposal-api-replace-fragile-worker-to-engine-seams.md`):
    /// [`worker_escalation_protocol_directive`], the two Bazel pre-push gate
    /// blurbs, and [`no_op_completion_directive`]'s pointer. `false` (the
    /// flag's registry default) reproduces the exact marker-only
    /// text; `true` teaches the `boss propose` verbs instead — see those
    /// functions' docs. This is the OTHER half of the flag: the engine's
    /// read path (`crate::completion::WorkerCompletionHandler::detect_and_file_worker_signals`)
    /// is gated by the same flag name read directly from
    /// `FeatureFlagsStore`; gating the prompt too is what makes "flag off"
    /// restore today's behavior exactly, prompt included — a worker must
    /// never be taught a verb the engine won't yet read proposals-first for.
    #[builder(default)]
    worker_signal_proposals_seam_enabled: bool,
    /// Already-merged `merge_order` siblings whose surfaces this forward-port
    /// must preserve (rendered lines). Empty for non-conflict revisions and
    /// for conflict revisions with no merged overlap partner.
    #[builder(default)]
    merge_order_preservation: &'a [String],
}

/// Render the `## STARTUP RECOVERY` block for a worker respawned after its
/// predecessor was interrupted.
///
/// ## What was wrong with the old block
///
/// It talked about exactly one thing — a branch the prior worker *might have
/// pushed* — and said nothing about the case that actually loses work: a
/// dirty working copy that was never committed, let alone pushed. Worse, its
/// fallback instruction was `jj new main@origin`, which **moves `@` off any
/// recovered uncommitted state**. A worker handed back a recovered tree,
/// finding no pushed branch, would follow the prompt and discard the very
/// work the recovery machinery had just saved.
///
/// ## What it says now
///
/// When the engine recovered state into this workspace it drops a marker
/// (`.boss/recovery-report.json`, see [`boss_engine_recovery::recovery_apply`]); `report` is
/// that marker, already filtered to this execution. The block then states, in
/// order:
///
/// 1. whether state was recovered, and how — in place by cube (jj history
///    intact) or replayed from a patch (uncommitted edits only);
/// 2. what exactly was restored, in files and line counts, so the worker can
///    check rather than guess;
/// 3. to **inspect before building on it** — recovered work is a crashed
///    worker's mid-thought, not a reviewed baseline;
/// 4. a fallback that is explicitly conditional on the working copy being
///    clean, so it can never discard recovered state.
///
/// A `patch_error` on the report means recovery FAILED. That case gets its
/// own paragraph telling the worker not to assume anything was resumed —
/// silence there would leave it guessing, which is how a "recovered" worker
/// quietly redoes everything or, worse, half-redoes it.
fn startup_recovery_block(
    prior_branch: &str,
    expected_branch_new: &str,
    report: Option<&boss_engine_recovery::recovery_apply::RecoveryReport>,
) -> String {
    use boss_engine_recovery::recovery_apply::RecoverySource;

    let mut block = String::from("## STARTUP RECOVERY\n\n");
    block.push_str(
        "This execution was respawned after the previous worker session was interrupted \
         (engine or UI crash). Treat everything below as a recovered mid-thought, not as a \
         reviewed starting point.\n\n",
    );

    // 1 + 2: what was recovered, and how.
    match report {
        Some(r) if r.patch_error.is_some() => {
            let err = r.patch_error.as_deref().unwrap_or_default();
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
        }
        Some(r) if r.source == RecoverySource::CubeInPlace => {
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
        }
        Some(r) => {
            // RecoverySource::Patch, applied successfully.
            let summary = r
                .applied
                .as_ref()
                .map(|a| a.summary())
                .unwrap_or_else(|| "nothing".to_string());
            let files = r
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
        None => {
            block.push_str(
                "The engine has no record of recovered uncommitted state for this run. Still \
                 check `jj status` before you start — if the working copy is NOT clean, it \
                 holds the prior worker's edits and you must inspect them rather than \
                 discard them.\n\n",
            );
        }
    }

    // 3: the pushed-branch half, unchanged in intent.
    block.push_str(&format!(
        "### Prior pushed branch\n\
         \n\
         The prior worker may also have pushed commits to `{prior_branch}` on the remote:\n\
         \n\
         ```\n\
         jj git fetch\n\
         jj edit {prior_branch}@origin   # resumes prior commits if the branch was pushed\n\
         ```\n\
         \n\
         If you resume that branch, continue from those commits and push using the NEW \
         expected branch name `{expected_branch_new}` (see the `expected branch name` line \
         in the execution context below). Do NOT reuse the prior branch name.\n\n",
    ));

    // 4: a fallback that cannot destroy recovered state.
    block.push_str(
        "### If there is no prior branch\n\
         \n\
         `jj edit` will fail if the prior worker never pushed. In that case:\n\
         \n\
         - If `jj status` shows a **dirty** working copy, that is your recovered work — \
           keep it and continue from where you are. **Do NOT run `jj new main@origin`**: it \
           moves `@` off the recovered state and is how this work gets lost a second time.\n\
         - Only if `jj status` shows the working copy is **clean** is it correct to start \
           fresh with `jj new main@origin`.\n\n",
    );

    block
}

pub(super) fn compose_execution_prompt(params: ExecutionPromptParams<'_>) -> String {
    let ExecutionPromptParams {
        execution,
        work_item,
        parent_project,
        workspace_path,
        cube_change_id,
        conflict_attempt,
        recovery_branch,
        ci_attempt,
        editorial_rules,
        pr_template_set,
        editorial_enabled,
        worker_signal_proposals_seam_enabled,
        merge_order_preservation,
    } = params;
    // Phase 9 #29: ci_remediation has its own templated prompt — embed
    // the engine-collected log excerpt, the failing-check set, and the
    // attempt-kind-specific playbook (rebase-first for `fix`, just the
    // retrigger CLI for `retrigger`).
    if execution.kind == ExecutionKind::CiRemediation
        && let Some(attempt) = ci_attempt
    {
        return compose_ci_remediation_prompt(
            execution,
            work_item,
            workspace_path,
            cube_change_id,
            attempt,
            /* test_command */ None,
        );
    }
    let mut prompt = String::new();
    prompt.push_str("You are a reusable Boss worker running one execution inside a dedicated repo workspace.\n");
    prompt.push_str("The current session cwd is already set to that workspace.\n");
    prompt.push_str("Do the work directly in the repository checkout before ending this run.\n");
    prompt.push_str("Avoid asking the human for permission during this pass; when you need review or direction, stop and summarize it clearly.\n\n");

    // If the chore already has a PR, inject a high-prominence resume
    // directive BEFORE the execution context so it outweighs the
    // workspace-rules default of `jj git fetch && jj new main`.
    let existing_pr_url = work_item_pr_url(work_item);
    if let Some(pr_url) = existing_pr_url {
        let pr_number = boss_github::pr_url::pr_number_from_url(pr_url)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into());
        prompt.push_str(&format!(
            "## RESUME EXISTING PR\n\
             \n\
             This task has an existing open PR (#{pr_number}) at {pr_url}.\n\
             You MUST add commits to that branch — do NOT start from `jj new main` and do NOT open a new PR.\n\
             \n\
             After leasing your workspace:\n\
             ```\n\
             jj git fetch\n\
             cube workspace goto --pr {pr_number}   # lands you on the PR branch\n\
             ```\n\
             Then make your changes on that branch and push:\n\
             ```\n\
             cube pr update --branch <branch-name>\n\
             ```\n\
             \n\
             If the branch cannot be resumed (deleted upstream, conflict you cannot resolve, etc.),\n\
             STOP and surface the blocker — do NOT silently open a parallel PR.\n\n",
        ));
    } else if let Some(prior_branch) = recovery_branch {
        // No PR URL on the work item, but the prior execution was orphaned
        // mid-flight (engine crash / UI crash). The prior worker may have
        // pushed commits to its expected branch before the session died.
        // Direct the new worker to resume that branch rather than starting
        // from main — fall back cleanly if the branch doesn't exist on
        // the remote.
        let expected_branch_new = crate::completion::expected_branch_name(
            &execution.id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
        );
        prompt.push_str(&startup_recovery_block(
            prior_branch,
            &expected_branch_new,
            boss_engine_recovery::recovery_apply::RecoveryReport::read_for(workspace_path, &execution.id).as_ref(),
        ));
    }

    let expected_branch = crate::completion::expected_branch_name(
        &execution.id,
        &execution.branch_naming,
        execution.worker_branch_prefix.as_deref(),
    );
    prompt.push_str("Execution context:\n");
    prompt.push_str(&format!("- execution id: `{}`\n", execution.id));
    prompt.push_str(&format!("- execution kind: `{}`\n", execution.kind));
    prompt.push_str(&format!("- workspace: `{}`\n", workspace_path.display()));
    prompt.push_str(&format!("- work item: `{}`\n", work_item_name(work_item)));
    // The "expected branch name" line directs the worker to push to a fresh
    // `boss/exec_<id>` bookmark and is correct only for executions that open
    // their OWN PR. A revision's deliverable is a new commit on the parent
    // PR's existing branch (see `compose_revision_directive`), so templating a
    // `boss/exec_*` branch name here would directly contradict that block's
    // "Do NOT create a `boss/exec_*` bookmark" instruction — and the revision
    // exec id has no corresponding branch anyway, so pushing it would create a
    // dangling branch no PR points at (issue #842). Omit the line for
    // revisions and let the revision directive be the only word on branching.
    // (`existing_pr_url` is the work item's PR; revisions carry the parent PR
    // on `execution.pr_url`, so this guard is checked independently.)
    if existing_pr_url.is_none() && execution.kind != ExecutionKind::RevisionImplementation {
        prompt.push_str(&format!(
            "- expected branch name: `{expected_branch}` — the engine reconstructs this from your execution id and uses it to find your PR. Push to this exact bookmark name.\n",
        ));
    }
    if let Some(cube_change_id) = cube_change_id {
        prompt.push_str(&format!("- local change: `{}`\n", cube_change_id));
    }
    // For any project-scoped task — the synthetic `kind = 'design'`
    // task plus ordinary `project_task` rows — the interesting
    // context (what the project is for, its goal) lives on the
    // parent project rather than on the task row. Surface it inline
    // so the worker has the project's name/goal/description to
    // anchor against, regardless of the execution kind.
    if let Some(project) = parent_project {
        prompt.push_str(&format!("- parent project: `{}`\n", project.name));
        if let Some(details) = project_details(project) {
            prompt.push_str("- project details:\n");
            prompt.push_str(details.trim_end());
            prompt.push('\n');
        }
    }
    if let Some(details) = work_item_details(work_item) {
        prompt.push_str("- details:\n");
        prompt.push_str(details.trim_end());
        prompt.push('\n');
    }
    prompt.push('\n');
    // Inject [editorial-rules] block when editorial controls are enabled (gated by
    // the `editorial_controls` feature flag — default OFF). When disabled the block
    // is omitted entirely so the worker gets no editorial instructions and the
    // PreToolUse hook is a no-op (nothing downstream enforces).
    if editorial_enabled {
        prompt.push_str(&render_editorial_rules_block(editorial_rules, pr_template_set));
        prompt.push('\n');
    }
    match execution.kind {
        ExecutionKind::ProjectDesign => {
            // A `design_postmortem` task reuses `ProjectDesign` for dispatch/
            // lifecycle purposes (same doc-PR handling, same repo resolution
            // — see `exec_status_helpers`), but its remit is the opposite of
            // an initial design task: update the *existing* doc to reflect
            // what shipped, not author a new one. Branch on the task's own
            // `kind` (not `execution.kind`) to give it the right directive.
            let is_postmortem = matches!(
                work_item,
                WorkItem::Task(t) | WorkItem::Chore(t) if t.kind == TaskKind::DesignPostmortem
            );
            if is_postmortem {
                prompt.push_str(&compose_design_postmortem_directive(
                    parent_project,
                    &crate::structured_output::default_path_string(&execution.id),
                ));
            } else {
                prompt.push_str(&compose_design_directive(parent_project));
            }
        }
        ExecutionKind::InvestigationImplementation => {
            prompt.push_str(&compose_investigation_directive());
        }
        ExecutionKind::RevisionImplementation => {
            prompt.push_str(&compose_revision_directive(
                execution,
                work_item,
                workspace_path,
                conflict_attempt,
                ci_attempt,
                merge_order_preservation,
                worker_signal_proposals_seam_enabled,
            ));
        }
        ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation => {
            prompt.push_str(
                "Expected outcome for this run:\n- implement the requested change in the workspace,\n- run relevant local validation when practical,\n- stop once the work is ready for a human to review or redirect.\n",
            );
            prompt.push_str(check_bypass_prohibition_text());
        }
        ExecutionKind::AnswerAgent => {
            // Read-only answer agent: it never touches the workspace or opens a
            // PR — its whole mandate is to answer the question and post one
            // thread reply. (The full answer-agent prompt is composed by P3b;
            // this arm keeps the generic composer sane and PR-free if reached.)
            prompt.push_str(
                "Expected outcome for this run:\n- read what you need to answer the question accurately,\n- post exactly one comprehensive reply to the comment thread,\n- take no other action — you are read-only.\n",
            );
        }
        ExecutionKind::AutomationTriage
        | ExecutionKind::CiRemediation
        | ExecutionKind::ConflictResolution
        | ExecutionKind::PrReview
        | ExecutionKind::ProductDesign => {
            prompt.push_str(
                "Expected outcome for this run:\n- make concrete progress on the assigned work,\n- leave the workspace in a reviewable state,\n- stop with a concise review summary.\n",
            );
        }
    }
    // Issue #804: code-touching implementation chores were pushing to PR
    // branches without a local build, and CI repeatedly caught errors a
    // local `bazel build`/`bazel test` of the touched targets would have
    // surfaced. Inject a hard pre-push build gate, but only when the
    // workspace is actually a Bazel workspace — non-Bazel repos
    // (gradle/maven/npm/…) must not see irrelevant build instructions.
    // Docs-only kinds (design/investigation) are excluded; revisions get
    // the gate inside `compose_revision_directive`.
    if matches!(
        execution.kind,
        ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation
    ) && let Some(gate) = bazel_prepush_gate_block(workspace_path, worker_signal_proposals_seam_enabled)
    {
        prompt.push_str(&gate);
    }
    if matches!(
        execution.kind,
        ExecutionKind::TaskImplementation
            | ExecutionKind::ChoreImplementation
            | ExecutionKind::ProjectDesign
            | ExecutionKind::InvestigationImplementation
    ) {
        // Acceptance criterion: the engine watches for a PR URL on the
        // run's branch when claude stops. If the worker stops without
        // pushing/opening one, the run is treated as incomplete and
        // the worker is automatically probed to produce a PR. Stating
        // this up front avoids the probe round-trip when the worker
        // would otherwise have stopped at "I made the changes" with
        // nothing pushed.
        //
        // AI #6 (incident 001): the branch name is engine-supplied —
        // `expected branch name` above. Workers MUST push to that
        // bookmark name, because the cold-path detector now reads
        // `gh pr list --head <expected-branch>` (a unique-by-construction
        // signal) instead of the structurally-unsafe shared-store jj
        // bookmark scan that produced the May 14 PR fan-out.
        //
        // When the chore already has a pr_url, the acceptance criterion
        // changes: the worker pushes to the existing PR branch instead of
        // creating a new one. The engine's staged-URL detector captures
        // the URL from `gh pr view` output at the end of the run.
        if let Some(pr_url) = existing_pr_url {
            let pr_number = boss_github::pr_url::pr_number_from_url(pr_url)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            prompt.push_str(&format!(
                "\nAcceptance criterion: when you believe the work is done, the deliverable is a PR URL.\n\
                 - Push your commits to the existing PR branch with `cube pr update --branch <branch-name>` (see the ## RESUME EXISTING PR block above). Do NOT open a new PR.\n\
                 - Confirm the PR is updated with `gh pr view {pr_number}` (pass `-R owner/repo` since bare gh calls need it in a jj workspace — use `jj git remote` to find the slug, or check the PR URL above).\n\
                 - Print the PR URL on its own line as the final thing in your final response so the engine can pick it up automatically.\n\
                 - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, you have made no changes — do NOT commit, push, or open a PR. Stop and explain what went wrong instead.\n",
            ));
        } else {
            prompt.push_str(&format!(
                "\nAcceptance criterion: when you believe the work is done, the deliverable is a PR URL.\n\
                 - Use the engine-supplied branch name from the `expected branch name` line above (`{expected_branch}`) when creating your bookmark — do NOT invent a different name.\n\
                 - Push your branch (`jj bookmark create {expected_branch} -r @`) and open a PR with `cube pr create --branch {expected_branch}` which pushes the branch and opens the PR in one step (jj-aware, no GIT_DIR needed). It is safe to retry: if a prior call already created the PR (e.g. your tool killed an earlier invocation on a timeout but the push had actually landed), it returns that PR's URL instead of erroring. Use `cube pr update --branch {expected_branch}` only when you have new commits to push onto an already-open PR.\n\
                 - **Never use `jj git push`, `git push`, or `gh pr create` directly** — always use `cube pr create` or `cube pr update`. A PreToolUse hook blocks direct push/PR-create attempts and redirects you to cube.\n\
                 - If a PR already exists for this branch (e.g. you are resuming work or addressing review comments), push your new commits to update it instead of opening a duplicate. Check with `gh pr view` from inside the workspace.\n\
                 - Print the PR URL on its own line as the final thing in your final response so the engine can pick it up automatically.\n\
                 - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, you have made no changes — do NOT commit, push, or open a PR. Stop and explain what went wrong instead.\n",
            ));
        }
        // Warn that PR creation is terminal — the engine reaps the worker
        // immediately after the PR is opened. Workers must finish everything
        // BEFORE opening the PR; no followup turn is possible.
        prompt.push_str(&pr_terminal_directive());
        // Issue #899: hand the worker the engine's CI-completion definition
        // so it stops once CI is effectively green rather than polling
        // forever on human-gated checks (e.g. LinkedIn's `Owner Approval`).
        prompt.push_str(&ci_monitoring_directive(execution));
        // Incident 2026-07-02 (exec_18b5243e65ff188_2d): teach the
        // full escalation/blocker marker syntax so a well-formed marker is
        // the default, not a lucky accident — see the function doc for why.
        prompt.push_str(&worker_escalation_protocol_directive(
            worker_signal_proposals_seam_enabled,
        ));
        // Teach chore/task workers the `[deferred-scope]` marker so a
        // deliberate scope narrowing is recorded, not just claimed in prose
        // ("filed as a followup") that nothing actually tracks (root cause:
        // PR #765).
        if matches!(
            execution.kind,
            ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation
        ) {
            prompt.push_str(&deferred_scope_directive());
        }
        // Give a fresh-PR chore/task implementation worker a SANCTIONED
        // way to terminate as "the work was already done". Without it, a worker
        // that correctly finds an empty diff stops and explains — and the
        // engine's Stop-boundary handler then nudges it to "produce a PR"
        // forever. Only for the no-existing-PR flow: when a PR already exists,
        // an empty diff means "already pushed", handled by the push-to-existing
        // path, not by closing the task as a no-op.
        if existing_pr_url.is_none()
            && matches!(
                execution.kind,
                ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation
            )
        {
            prompt.push_str(&no_op_completion_directive(worker_signal_proposals_seam_enabled));
        }
    }
    // Attentions creation pipeline (design: attentions.md): implementation
    // workers may surface out-of-scope follow-on work as a `FOLLOWUPS:` block
    // the engine parses at completion. Design workers use the questions
    // manifest instead, so they are excluded here.
    if matches!(
        execution.kind,
        ExecutionKind::TaskImplementation
            | ExecutionKind::ChoreImplementation
            | ExecutionKind::InvestigationImplementation
            | ExecutionKind::RevisionImplementation
    ) {
        prompt.push_str(&followups_emission_block(
            &crate::structured_output::default_path_string(&execution.id),
        ));
    }
    prompt.push_str("\nRespond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
}

/// True when `workspace_path` is the root of a Bazel workspace — i.e. a
/// `MODULE.bazel`, `WORKSPACE`, or `WORKSPACE.bazel` marker file sits at
/// the root. Bazel ownership is what gates the pre-push build
/// requirement (issue #804): many target repos are gradle/maven/npm/etc.
/// and must not be told to run `bazel build`.
fn is_bazel_workspace(workspace_path: &Path) -> bool {
    ["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel"]
        .iter()
        .any(|marker| workspace_path.join(marker).exists())
}

/// Pre-push build gate for Bazel workspaces (issue #804). Workers were
/// pushing code-touching chores to PR branches without a local build,
/// and CI repeatedly caught errors a local `bazel build`/`bazel test` of
/// the touched targets would have surfaced (stale crate_universe
/// lockfiles, gazelle validation, clippy `await_holding_lock`). The
/// loose "please verify" prose in chore descriptions did not hold, so
/// this states the requirement as a hard gate in the worker prompt.
///
/// Returns `None` for non-Bazel repos so the block is only injected when
/// bazel actually owns the workspace. `seam_enabled` selects which failure-
/// escalation sentence [`bazel_prepush_gate_text`] renders — see that
/// function's doc.
fn bazel_prepush_gate_block(workspace_path: &Path, seam_enabled: bool) -> Option<String> {
    if !is_bazel_workspace(workspace_path) {
        return None;
    }
    Some(bazel_prepush_gate_text(seam_enabled))
}

/// The Bazel pre-push build-gate prompt block, independent of any
/// filesystem probe. Extracted so the SSH remote adapter can append it
/// when a *remote* workspace is a Bazel workspace: [`is_bazel_workspace`]
/// only probes the local filesystem, so a remote workspace path never
/// matches and the gate has to be injected from the result of an
/// over-SSH marker probe instead.
///
/// `seam_enabled` mirrors `worker_signal_proposals_seam` (see
/// [`worker_escalation_protocol_directive`]): `true` points a build-gate
/// failure at `boss propose blocked`; `false` reproduces the pre-migration
/// `[blocked]` marker sentence, so a worker on the flag-off path is never
/// told to call a verb the engine won't yet honor proposals-first.
pub(crate) fn bazel_prepush_gate_text(seam_enabled: bool) -> String {
    let failure_sentence = if seam_enabled {
        "If the build or tests fail, time out, or you cannot make them pass within this run, do NOT push red code and do NOT idle waiting on them. Call `boss propose blocked --reason \"...\"` naming the failing/timed-out command and its output, and stop (see \"If you are blocked or the work is bigger than estimated\" below for the exact syntax). Escalating a blocker is correct; pushing a known-broken branch — or hanging on a wedged build — is not.\n"
    } else {
        "If the build or tests fail, time out, or you cannot make them pass within this run, do NOT push red code and do NOT idle waiting on them. Emit a `[blocked] reason=\"...\"` marker in your final response naming the failing/timed-out command and its output, and stop (see \"If you are blocked or the work is bigger than estimated\" below for the exact syntax). Escalating a blocker is correct; pushing a known-broken branch — or hanging on a wedged build — is not.\n"
    };
    format!(
        "\n## Pre-push build gate (Bazel workspace)\n\
         \n\
         This repository is a Bazel workspace (a `MODULE.bazel`/`WORKSPACE` marker was found at the workspace root). Before you push a branch or update a PR with code changes, you MUST run a clean local build and test of what you touched and confirm both pass. \"I think it should work\" or \"the change looks correct\" is NOT a substitute for running the build — repeated rounds of CI breakage have come from workers skipping this step.\n\
         \n\
         Required before pushing:\n\
         - `bazel build` every target you changed and `bazel test` their tests. Use `bazel query` to resolve the target labels covering the files you edited if you are unsure which they are.\n\
         - If reverse dependencies are quick to enumerate, build them too so you don't break consumers: `bazel query 'rdeps(//..., <changed-target>)'`, then build the results.\n\
         - If a CI workflow file exists (`.github/workflows/*.yml`), open it and mirror the exact bazel target set it builds/tests (these repos typically run `bazel build //...` or a curated rollup). Run that same command locally so your gate matches what CI will enforce.\n\
         - Both `bazel build` and `bazel test` must finish clean — exit 0, no build errors, no failing tests, no clippy/lint failures — before you push.\n\
         \n\
         Run the build gate in the FOREGROUND and read its exit code directly. Do NOT background bazel (no `&`, no `run_in_background`, no redirecting to a log file you then poll) and then idle in a self-paced wait-loop \"until the gate is green\". If the bazel server wedges (host contention, a hung toolchain), those log files may never appear and the completion notification never arrives — you will wait forever with no way out, stranding your slot. If you need an upper bound, wrap the command itself in a timeout (e.g. `timeout 1800 bazel test //...`) so it returns control to you on expiry; on a timeout, treat it as a blocker (below), do not retry-and-idle.\n\
         \n\
         {failure_sentence}"
    )
}

/// Pre-push gate for a **conflict-resolution** revision, when the
/// workspace is a Bazel workspace. Returns `None` for non-Bazel repos.
///
/// This deliberately differs from [`bazel_prepush_gate_block`]: a
/// conflict-resolution revision's job is to make the PR mergeable again
/// (the *merge-correctness* gate), not to certify the whole PR's test
/// suite. The full `bazel test //...` belongs to the PR's own CI, which
/// runs on the branch the worker pushes. Blocking the push behind a long
/// or flaky full-suite run is exactly how a correct resolution gets
/// stranded unpushed and lost on reap (the loop this fix addresses).
///
/// The verify gate is NOT skipped: the merged code must COMPILE
/// (`bazel build` of the touched/upstream targets) and any rebase-
/// invalidated generated artifact (e.g. `MODULE.bazel.lock`) must be
/// regenerated before pushing. Tests run post-push in CI.
fn bazel_conflict_resolution_gate_block(workspace_path: &Path, seam_enabled: bool) -> Option<String> {
    if !is_bazel_workspace(workspace_path) {
        return None;
    }
    Some(bazel_conflict_resolution_gate_text(seam_enabled))
}

/// The conflict-resolution pre-push gate prompt block, independent of any
/// filesystem probe (so the SSH remote adapter can inject it after an
/// over-SSH marker probe). See [`bazel_conflict_resolution_gate_block`]
/// for why this is build-before-push rather than build-and-test-before-push,
/// and [`bazel_prepush_gate_text`] for what `seam_enabled` selects.
pub(crate) fn bazel_conflict_resolution_gate_text(seam_enabled: bool) -> String {
    let failure_sentence = if seam_enabled {
        "If `bazel build` fails (the merge does not compile) and you cannot make it compile within this run, do NOT push. Fix the resolution, or — if it needs a human decision — follow the stop conditions below. Do NOT idle waiting on a wedged build; call `boss propose blocked --reason \"...\"` naming the failure and stop.\n"
    } else {
        "If `bazel build` fails (the merge does not compile) and you cannot make it compile within this run, do NOT push. Fix the resolution, or — if it needs a human decision — follow the stop conditions below. Do NOT idle waiting on a wedged build; emit a `[blocked] reason=\"...\"` marker naming the failure and stop.\n"
    };
    format!(
        "\n## Pre-push gate for conflict resolution (Bazel workspace) — merge correctness first, then push\n\
         \n\
         This repository is a Bazel workspace. For a conflict-resolution revision the gate you MUST clear before pushing is **merge correctness**, not the full test suite.\n\
         \n\
         Required BEFORE you push (step 4):\n\
         - Regenerate any generated/lock artifact the rebase invalidated and include it in your commit. The common one is `MODULE.bazel.lock`: run `bazel mod deps --lockfile_mode=update` (or build any target, which refreshes it) and stage the result.\n\
         - `bazel build` the targets your resolution touched AND the targets the rebased-in upstream change touches. Use `bazel query` to resolve labels if unsure. The merged code MUST COMPILE — a conflict resolution that does not build is wrong and must not be pushed.\n\
         - Run the build in the FOREGROUND with a timeout (e.g. `timeout 1800 bazel build <targets>`) and read its exit code directly. Do NOT background it and idle in a wait-loop.\n\
         \n\
         Then PUSH (step 4) as soon as the build is clean. Do NOT block the push on a full `bazel test //...`.\n\
         \n\
         Why push before the full test suite: making the PR mergeable again is the conflict-resolution step's deliverable. The PR's own CI runs the full `bazel test` suite on the branch you push — that is where test regressions are caught and remediated, NOT a precondition for landing the resolution. Stalling the push behind a long or flaky full-suite run is exactly how a correct resolution gets stranded and never reaches the PR.\n\
         \n\
         After pushing you MAY run `bazel test` on the affected targets as a courtesy and report what you saw, but the push must not wait on it.\n\
         \n\
         {failure_sentence}"
    )
}

/// Hard constraint text forbidding check/CI bypasses. Injected into every
/// prompt surface where a worker might encounter a failing check or CI failure.
fn check_bypass_prohibition_text() -> &'static str {
    "\n**Hard constraint — fix failing checks at the root cause; never bypass them.**\n\n\
     Forbidden moves (each is a bypass, not a fix — do NOT do any of them):\n\
     - Adding a file to a check exclusion or allowlist (`CHECKS.yaml` `exclude_files`, checkleft excludes, lint-disable comments, etc.) to suppress the failure.\n\
     - Setting `allow_bypass`, using an override flag, or invoking any bypass/override mechanism on a check.\n\
     - Passing `--no-verify` / skipping git hooks; adding broad `#[allow(...)]` / `// swiftlint:disable` / `# noqa` annotations solely to suppress a warning or error.\n\
     - Deleting, `#[ignore]`-ing, `xfail`-ing, skipping, or weakening assertions in a failing test to make it pass.\n\
     - Raising a threshold or limit (e.g. `max_lines` in a file-size check) solely to accommodate the offending file without reducing its size.\n\n\
     Required behavior: fix the real problem — split the oversized file, fix the lint/compile error, fix the test failure, resolve the root cause. If a check genuinely SHOULD be relaxed (a legitimately needed exclusion or threshold change), that is a human decision — STOP and surface it for operator approval with full justification. Do not decide this autonomously.\n"
}

/// Render the `[editorial-rules]` block for the worker prompt (chore #5).
///
/// Always rendered — even for default-config products — because the baked-in
/// identifier-redaction rules apply to every execution. The optional
/// instructions / template / enforcement sections are only included when the
/// product has non-default editorial configuration (instructions set or
/// template_policy != Off). This matches the acceptance criterion: default-config
/// products get baked-in rules only; configured products get instructions +
/// template + enforcement banner.
fn render_editorial_rules_block(
    editorial_rules: Option<&EditorialRules>,
    pr_template_set: &crate::pr_template::PrTemplateSet,
) -> String {
    let instructions = editorial_rules
        .and_then(|r| r.instructions.as_deref())
        .filter(|s| !s.is_empty());
    let template_policy = editorial_rules.map(|r| r.template_policy.clone()).unwrap_or_default();
    let is_configured = instructions.is_some() || !matches!(template_policy, TemplatePolicy::Off);

    let mut out = String::new();
    out.push_str("[editorial-rules]\n");
    out.push_str("**Editorial rules for PRs / GitHub comments in this product.**\n");
    out.push_str(
        "Apply these rules to every PR title, PR body, PR / issue comment, \
         commit-message body, and merge-conflict note you write for this run.\n\n",
    );
    out.push_str("Baked-in rules (always apply):\n");
    out.push_str(
        "- Do not mention Boss execution / project / task / chore identifiers \
         in user-facing text. The shapes are `exec_…`, `proj_…`, `task_…`, \
         `chg_…`. They are internal vocabulary that humans on this repo have no \
         context for.\n",
    );
    out.push_str(
        "- Do not refer to \"Boss worker\", \"the engine\", \"the coordinator\", \
         \"cube workspace\", or \"work item\" in user-facing text — these are \
         internal Boss vocabulary.\n",
    );
    out.push_str(
        "- When referring to your branch in PR text, say \"this branch\" rather \
         than its full name — the branch name is engine bookkeeping (it associates \
         the PR with its originating execution) and is not meaningful to human \
         reviewers.\n",
    );

    if is_configured {
        if let Some(instr) = instructions {
            out.push_str("\nProduct-specific rules (configured on this product):\n");
            out.push_str(instr.trim_end());
            out.push('\n');
        }

        let policy_label = match template_policy {
            TemplatePolicy::Off => None,
            TemplatePolicy::Advise => Some("Advise"),
            TemplatePolicy::Enforce => Some("Enforce"),
        };
        if let Some(label) = policy_label {
            let template_path = pr_template_set
                .default_template
                .as_ref()
                .map(|t| t.source_path.display().to_string())
                .or_else(|| {
                    let mut stems: Vec<&str> = pr_template_set.named_templates.keys().map(String::as_str).collect();
                    stems.sort();
                    stems
                        .first()
                        .map(|stem| format!(".github/PULL_REQUEST_TEMPLATE/{stem}.md"))
                })
                .unwrap_or_else(|| ".github/PULL_REQUEST_TEMPLATE.md".to_string());
            out.push_str(&format!("\nTemplate policy: {label}: see {template_path}\n"));
            if !pr_template_set.is_empty() {
                out.push_str(
                    "The PR body must follow the structure of the template (rendered below), \
                     regardless of the final-response sectioning rules.\n",
                );
                let has_multiple = pr_template_set.named_templates.len() > 1
                    || (pr_template_set.default_template.is_some() && !pr_template_set.named_templates.is_empty());
                for tmpl in pr_template_set.all_templates() {
                    if has_multiple {
                        out.push_str(&format!("\nTemplate (`{}`):\n", tmpl.source_path.display()));
                    }
                    out.push_str("\n```\n");
                    out.push_str(tmpl.text.trim_end());
                    out.push_str("\n```\n");
                }
            }
        }

        out.push_str("\nEnforcement:\n");
        out.push_str(
            "The engine's PreToolUse hook intercepts `gh pr create`, `gh pr edit`, \
             `gh pr comment`, `gh pr review`, and `gh issue comment` invocations. \
             If your body / title violates a rule, the call is denied or rewritten and \
             you will see feedback. Comply on the first try when you can — denials cost \
             a worker turn.\n",
        );
    }

    out.push_str("[/editorial-rules]\n");
    out
}

/// Directive that warns workers PR creation is terminal: the engine reaps
/// them immediately after the PR is opened. No followup turn is possible.
/// Workers must finish all work — including consuming any in-flight reviews
/// they started — BEFORE opening the PR. Incident: a worker opened a PR,
/// then tried to wait for background review subagents and address their
/// findings as followup commits. The engine terminated the worker the moment
/// the PR was created, so the review was never consumed. This universal
/// guidance applies to every execution kind and prevents that pattern.
fn pr_terminal_directive() -> String {
    let mut out = String::new();
    out.push_str("\n## Important: PR creation is your terminal act\n\n");
    out.push_str(
        "Opening the PR is the LAST thing you do. The engine reaps you immediately after the PR is created.\n\n",
    );
    out.push_str("You will NOT get another turn after `gh pr create` / `cube pr create` (or `cube pr update` for an existing PR). Do not plan followup commits, do not defer work to \"after the PR\", do not open the PR while background work (subagent workflows, backgrounded builds, code reviews) is still in flight expecting to consume its results.\n\n");
    out.push_str("Therefore: finish everything — including consuming any review/self-review findings you started — BEFORE you open the PR. If a background review is still running and you care about its results, wait for it and address all findings FIRST, then open the PR. If you don't intend to wait, don't start the review.\n");
    out
}

/// Worker Stop-boundary escalation/blocker protocol directive. `seam_enabled`
/// mirrors the `worker_signal_proposals_seam` feature flag — the same flag
/// [`crate::completion::WorkerCompletionHandler::detect_and_file_worker_signals`]
/// reads for the engine's read path, threaded here so the two halves of the
/// migration move together: a worker must never be taught a verb the engine
/// won't yet read proposals-first for, and flipping the flag off must
/// restore today's behavior exactly, prompt included, not just the engine's
/// read side.
///
/// `seam_enabled = true` documents the two sanctioned `boss propose` verbs a
/// worker calls when it cannot proceed unassisted: `effort-escalation` (the
/// work is bigger than estimated) and `blocked` (a human/coordinator
/// decision is needed), plus the `[blocked]` marker retained as a bootstrap
/// fallback of last resort. `boss propose` validates synchronously, so a
/// malformed call fails with a typed error the worker can fix and retry
/// in-run, instead of a marker whose fields are only checked long after the
/// worker could do anything about it. The `[blocked]` marker itself is not
/// deleted — the design keeps it indefinitely as "the channel of last
/// resort, precisely because it must work when the mechanism itself is
/// broken" — but it is documented here as exactly that: a bootstrap
/// fallback, not a second normal-path channel. `[effort-escalation]` has no
/// such carve-out and is not taught here at all; the engine still accepts
/// it as a counted legacy fallback (`crate::worker_escalation`) so a stray
/// marker from an older transcript or a worker that ignores this directive
/// is still surfaced, never silently dropped, but new workers are only ever
/// taught the verb.
///
/// `seam_enabled = false` reproduces the pre-migration directive verbatim:
/// both `[effort-escalation]` and `[blocked]` as markers, no `boss propose`
/// mention. Incident 2026-07-02 (`exec_18b5243e65ff188_2d`) is why
/// the marker syntax is spelled out explicitly rather than left implicit —
/// a worker hit a bazel blocker it could not resolve, did the right thing
/// by stopping instead of pushing broken code, and emitted a bare
/// `[effort-escalation]` line with neither `requested_level` nor `reason`,
/// which the coordinator's documented parser treats as malformed.
///
/// See [`crate::worker_escalation`] for the legacy marker parser and
/// [`crate::completion::WorkerCompletionHandler::detect_and_file_worker_signals`]
/// for what the engine does with either channel: files a coordinator-visible
/// attention item and pauses the "produce a PR" auto-nudge until it is
/// resolved (a coordinator probe on this run resolves it — see
/// [`crate::work::WorkDb::resolve_worker_signal_attentions_for_execution`]).
pub(crate) fn worker_escalation_protocol_directive(seam_enabled: bool) -> String {
    if !seam_enabled {
        return "\n## If you are blocked or the work is bigger than estimated\n\n\
     Two sanctioned markers, each on its own line in your final response, tell the coordinator \
     you need help. Emitting one is always the right move over pushing broken/unvalidated work \
     or idling silently:\n\n\
     - **`[effort-escalation] requested_level=<level> reason=\"<why>\"`** — the assigned work \
     needs more effort than it was classified at. `<level>` is one bareword, one of \
     `trivial|small|medium|large|max`. `<why>` is a double-quoted, one-line reason. Both fields \
     are required. Example:\n\n\
     ```\n\
     [effort-escalation] requested_level=large reason=\"ran into a multi-subsystem race; description didn't mention the engine/app boundary\"\n\
     ```\n\n\
     - **`[blocked] reason=\"<why>\"`** — you cannot proceed without a human/coordinator \
     decision: a build failure you can't resolve, an ambiguous requirement, conflicting \
     instructions, a missing credential. `<why>` is a double-quoted, one-line reason. Example:\n\n\
     ```\n\
     [blocked] reason=\"bazel build fails with E0583 for a newly added file, survives clean --expunge; need guidance or explicit direction before proceeding\"\n\
     ```\n\n\
     A marker missing `requested_level=`/`reason=\"...\"` is still detected but flagged \
     malformed to the coordinator — include both fields so it's processed automatically instead \
     of by hand. Do NOT stop silently or push code you know is broken to work around a blocker: \
     emit the marker instead. The engine files it as an attention item for the coordinator and \
     pauses the auto-nudge loop for this run until it acks, so you will not be re-prompted to \
     \"produce a PR\" while a marker is pending.\n"
            .to_string();
    }
    "\n## If you are blocked or the work is bigger than estimated\n\n\
     Two verbs on the `boss` CLI tell the coordinator you need help. Calling one is always the \
     right move over pushing broken/unvalidated work or idling silently. Submission is synchronous \
     and validated immediately, so a malformed call fails right away with a typed error you can fix \
     and retry — unlike a marker line, which the engine only reads long after you've moved on:\n\n\
     - **`boss propose effort-escalation --level <level> --reason \"<why>\"`** — the assigned work \
     needs more effort than it was classified at. `<level>` is one of \
     `trivial|small|medium|large|max`. Example:\n\n\
     ```\n\
     boss propose effort-escalation --level large --reason \"ran into a multi-subsystem race; description didn't mention the engine/app boundary\"\n\
     ```\n\n\
     - **`boss propose blocked --reason \"<why>\"`** — you cannot proceed without a \
     human/coordinator decision: a build failure you can't resolve, an ambiguous requirement, \
     conflicting instructions, a missing credential. Example:\n\n\
     ```\n\
     boss propose blocked --reason \"bazel build fails with E0583 for a newly added file, survives clean --expunge; need guidance or explicit direction before proceeding\"\n\
     ```\n\n\
     Either call files a coordinator-visible attention item immediately and pauses the \"produce a \
     PR\" auto-nudge loop for this run until a coordinator acks it, so you will not be re-prompted \
     to \"produce a PR\" while one is pending. Do NOT stop silently or push code you know is broken \
     to work around a blocker: call `boss propose` instead.\n\n\
     **Bootstrap fallback only:** if `boss propose` itself is unreachable (the mechanism is down, \
     the socket is gone, or you are a remote worker with no local peer to attribute the call to), \
     fall back to a bare `[blocked] reason=\"<why>\"` line on its own line in your final response — \
     the one marker kept specifically because it must still work when the mechanism itself is \
     broken. If the underlying problem is an effort escalation rather than a blocker, state the \
     requested level in the reason (e.g. `[blocked] reason=\"boss propose unreachable; requesting \
     effort escalation to large — <why>\"`) — this bootstrap channel is the only one guaranteed to \
     work when `boss propose` itself is down, so it carries both signal kinds rather than teaching \
     a second marker grammar back. Do not use it once `boss propose` has already succeeded for this \
     signal; it is a last resort, not a second channel.\n"
        .to_string()
}

/// `[deferred-scope]` marker protocol directive (root-caused to PR #765).
/// A worker that deliberately narrows its own scope — delivers part of the
/// brief and consciously defers a piece of it, rather than merely running
/// out of turns — had no sanctioned way to record that decision: task
/// completion is binary (PR merged => done) and nothing reconciles
/// delivered scope against the brief. The PR #765 worker wired part
/// of a feature, deferred the rest because it needed new data plumbing, and
/// wrote "I've filed it as a followup" in the PR body — workers cannot file
/// anything, so the remainder silently died until an operator noticed weeks
/// later. This directive gives deferred scope a parseable channel mirroring
/// `[effort-escalation]`'s grammar; see [`crate::deferred_scope`] for the
/// parser and
/// [`crate::completion::WorkerCompletionHandler::detect_and_record_deferred_scope`]
/// for what the engine does with it: appends a durable audit line to the
/// work item's description and files a coordinator-visible attention item.
pub(crate) fn deferred_scope_directive() -> String {
    "\n## If you deliver less than the brief asks: declare the gap\n\n\
     If you consciously decide to narrow scope — implement part of what was asked and \
     deliberately leave a piece undone (it needs plumbing/data/access this run doesn't have, \
     it's a genuinely separate concern, etc.) rather than doing it — emit one line per deferred \
     item in your final response:\n\n\
     ```\n\
     [deferred-scope] summary=\"<what you did not deliver>\" reason=\"<why you deferred it>\"\n\
     ```\n\n\
     Both fields are double-quoted and required. Example:\n\n\
     ```\n\
     [deferred-scope] summary=\"wiring for the third data source\" reason=\"needs a new ingestion pipeline; out of scope for this wiring-only chore\"\n\
     ```\n\n\
     Do NOT write \"filed as a followup\", \"tracked separately\", or similar in your PR body or \
     summary as a substitute — you have no ability to file or track anything, that sentence would \
     simply be false, and the deferred work will be silently lost with no record. The \
     `[deferred-scope]` marker is the channel that actually creates one: it is recorded against \
     this task and surfaced to a human, who decides whether to spin up a followup or accept the \
     gap. This is distinct from the followups mechanism above, which proposes brand-new \
     out-of-scope work you noticed — use `[deferred-scope]` specifically for work the brief asked \
     for that you did not deliver.\n\n\
     **The marker is the only sanctioned channel for declaring deferred scope — prose is not \
     enough.** If your PR body, a summary section, or your final response says anything that \
     states or implies narrowed scope — \"deferred\", \"not included in this PR\", \"left for a \
     future task\", \"out of scope for now\", a \"## Deferred\" heading, or similar — every item \
     it names MUST also have a matching `[deferred-scope]` line in your final response. A prose \
     deferral section with no matching markers is a protocol violation: reviewers are instructed \
     to flag it, and it will be flagged. A \"## Deferred\" section in the PR body is fine as \
     human-readable prose, but only in addition to the markers, never instead of them — the \
     marker costs one line and is parsed even if malformed, so there is no excuse to skip it.\n"
        .to_string()
}

/// Sanctioned no-op completion directive. A `chore_implementation`
/// / `task_implementation` worker sometimes investigates and finds the work
/// is *already done* — the change is already on `main`, so `jj diff -r @` is
/// empty and there is nothing to commit/push/open a PR for. That is a
/// legitimate success, not a failure. Before this directive the worker was
/// told only to "stop and explain", and the engine's Stop-boundary handler
/// then read the empty branch as "stopped without producing a PR" and nudged
/// it to `gh pr create` — the two instructions were in direct conflict and
/// the worker churned against the nudge until the breaker parked it.
///
/// This block reframes the already-done empty-diff case as a success and
/// gives the worker an unambiguous terminal signal: emit the
/// [`NO_CHANGES_NEEDED`](crate::no_op_signal::NO_CHANGES_NEEDED_MARKER) marker
/// on its own line and stop. The engine accepts that marker (combined with a
/// genuinely empty contribution — no PR pushed, none bound) as a clean
/// terminal and closes the task as done WITHOUT a PR, sending no nudge. The
/// marker is the *only* sanctioned way to signal this; a worker that simply
/// stops without it is still nudged, so this must NOT be used to bail out of
/// work that is merely hard or blocked.
fn no_op_completion_directive(seam_enabled: bool) -> String {
    let marker = crate::no_op_signal::NO_CHANGES_NEEDED_MARKER;
    let blocked_pointer = if seam_enabled {
        "call `boss propose blocked --reason \"...\"` instead"
    } else {
        "emit a `[blocked] reason=\"...\"` marker instead"
    };
    let mut out = String::new();
    out.push_str("\n## If the work is already done: signal a sanctioned no-op\n\n");
    out.push_str(
        "Run `jj diff -r @` before you conclude. If the diff is empty because the work is ALREADY \
         DONE — the change is already present on `main` (e.g. another PR landed it), and there is \
         genuinely nothing left to change — that is a legitimate, SUCCESSFUL outcome, not a \
         failure.\n\n",
    );
    out.push_str(&format!(
        "In that case, do NOT commit, push, or open a PR, and do NOT push an empty/no-op PR to \
         manufacture a deliverable. Instead, emit a line containing exactly `{marker}` as the \
         final line of your response, then stop. The engine recognizes this marker and closes the \
         task as already-done — no PR is required and you will not be nudged to produce one.\n\n"
    ));
    out.push_str(&format!(
        "This replaces the generic \"stop and explain what went wrong\" for the already-done case: \
         an empty diff because the work is done is a success terminal, not an error. Do NOT emit \
         `{marker}` to abandon work you simply found hard or are blocked on — if you are blocked, \
         {blocked_pointer} (see \"If you are blocked or the work is bigger than estimated\" above), \
         and the engine will route it to the coordinator without nudging you to produce a PR.\n"
    ));
    out
}

/// Post-PR CI-monitoring directive (issue #899). A worker that opens a
/// PR and then sits in a `gh pr checks` poll-loop "until every check is
/// green" never completes under CI models where some required checks are
/// gated on a human action and never auto-resolve — LinkedIn's
/// `Owner Approval` is the canonical case. The engine's merge poller
/// already classifies CI correctly for these orgs: it partitions the
/// human-gated checks out of the CI rollup
/// (`merge_poller::review_signal_checks_for_owner`) before deciding the
/// PR is "effectively green", and auto-transitions the task to Review.
/// The worker had no share of that knowledge and so polled forever.
///
/// This block hands the worker the *same* CI-completion definition the
/// engine uses, sourced from the *same* table — when the PR's org ships
/// human-gated checks, they are named verbatim from
/// `review_signal_checks_for_owner` so the worker's "don't wait on these"
/// list and the engine's "these don't block CI-clean" set cannot drift.
fn ci_monitoring_directive(execution: &WorkExecution) -> String {
    let mut out = String::new();
    out.push_str("\n## After the PR is open: do not babysit CI\n\n");
    out.push_str(
        "Once your branch is pushed and the PR exists, your deliverable is done — print the PR URL and stop. Do NOT sit in a loop polling `gh pr checks` / `gh pr view` waiting for every check to turn green. That loop can run forever and strands your slot.\n\n",
    );
    out.push_str(
        "Why this is safe: the engine polls this PR's CI on its own cadence and auto-transitions the task to Review the moment CI is *effectively green*. \"Effectively green\" matches the engine's own definition — every required CI check has reached a passing terminal state (`SUCCESS`, `NEUTRAL`, or `SKIPPED`). It deliberately does NOT require checks that are gated on a human action and never resolve from CI alone; waiting on those is waiting forever.\n\n",
    );
    // Name the human-gated checks for this PR's org from the *same* table
    // the engine's CI classifier reclassifies on, so the two lists are
    // sourced once. Empty for orgs without review-signal rules — then the
    // general guidance above stands on its own.
    if let Ok(slug) = crate::completion::parse_repo_slug(&execution.repo_remote_url) {
        let owner = slug.split('/').next().unwrap_or("");
        let names = crate::merge_poller::review_signal_checks_for_owner(owner);
        if !names.is_empty() {
            let rendered = names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ");
            out.push_str(&format!(
                "This PR's org (`{owner}`) ships required check(s) that are human-gated and never auto-resolve from CI: {rendered}. The engine's CI-completion check treats them as NOT blocking — they stay pending until a human approves. You must do the same: their pending/running state is not a reason to keep this run alive.\n\n",
            ));
        }
    }
    out.push_str(
        "A required CI check that has genuinely *failed* (not merely pending) is different — fix it and push, or escalate per the build-gate rules above. But a still-running or human-gated check never blocks your completion.\n",
    );
    out
}

/// Markdown structure guidance shared by design and investigation docs.
/// Generated docs frequently render their intro as a single wall-of-text
/// paragraph: workers write metadata and framing as consecutive
/// single-newline lines, and a single newline is a soft wrap in Markdown —
/// it folds into one paragraph on render even though the source looks fine
/// line-by-line in an editor. Stated explicitly here because it looks
/// correct in the editor, so workers never self-correct without being told.
fn doc_structure_conventions_block() -> String {
    let mut out = String::new();
    out.push_str("- **Markdown structure — avoid wall-of-text rendering.** A single newline is a soft wrap in Markdown: consecutive non-blank lines collapse into one paragraph when rendered, even though they look like separate lines in an editor.\n");
    out.push_str("  - put metadata (Date, Task/provenance, related work items) in a bullet list or table immediately after the H1 — never as consecutive prose lines.\n");
    out.push_str("  - put a blank line between every logical block (metadata, framing, method, each finding, the verdict). Single newlines between blocks will smoosh them together.\n");
    out.push_str("  - give the verdict/TL;DR its own short section or paragraph — never embed it mid-paragraph.\n");
    out.push_str("  - keep the first paragraph after the title to 2-3 sentences; move framing and method detail into later sections.\n");
    out
}

/// Directive block for the synthetic `kind = 'design'` task that the
/// engine auto-creates with every project. Without this block the
/// `project_design` worker only sees the generic "draft or update a
/// repo-backed design artifact" line and frequently starts
/// implementing — observed against worker O'Brien
/// (exec_18aebf0caa1187e8_b). State up front that the deliverable is
/// a markdown design doc (not code), name the canonical path, and
/// list the section shape the reader expects so the worker doesn't
/// invent its own.
fn compose_design_directive(parent_project: Option<&Project>) -> String {
    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str("- the deliverable is a **design document**, not an implementation. Do not edit code, do not start prototyping, do not open partial implementation PRs.\n");
    out.push_str("- the PR for this run contains **only the design doc** (one new or updated markdown file). If you find yourself touching `.rs`, `.ts`, `.swift`, build files, or anything else, stop — you are out of scope.\n");
    if let Some(path_line) = canonical_design_doc_path_line(parent_project) {
        out.push_str(&path_line);
    }
    out.push_str(&doc_structure_conventions_block());
    out.push_str("- the design must cover, at minimum, these sections (use these as headings unless the parent project's description specifies a different shape):\n");
    out.push_str("  - **Goals** — what this project is trying to achieve, pulled from the parent project's goal/description above.\n");
    out.push_str("  - **Non-goals** — what is explicitly out of scope so reviewers don't have to guess.\n");
    out.push_str("  - **Alternatives considered** — at least two distinct approaches and why they were not chosen.\n");
    out.push_str("  - **Chosen approach** — the design itself, with enough detail that a follow-up implementation task can be filed against it.\n");
    out.push_str("  - **Risks / open questions** — anything the author wants a human reviewer to land on before implementation starts.\n");
    out.push_str("  - **Proposed implementation task breakdown** — this section is **required** and must be the final section of the doc. It is the machine-findable handoff to scheduling (see below).\n");
    out.push_str("- the **Proposed implementation task breakdown** section must:\n");
    out.push_str("  - use exactly that heading (`## Proposed implementation task breakdown`) so a downstream parser can locate it reliably.\n");
    out.push_str("  - list PR-sized tasks in dependency order, where each entry contains:\n");
    out.push_str("    - a short **task name** (one line).\n");
    out.push_str("    - a one-paragraph **scope** description.\n");
    out.push_str("    - an **effort hint**: one of `trivial | small | medium | large`.\n");
    out.push_str("    - **explicit dependencies** — which other entries in this list gate this one (use the task names; \"none\" if it can start immediately).\n");
    out.push_str("    - a **scope tag** — exactly one of `Scope: in-scope` or `Scope: deferred (future / not a v1 blocker)`. Use this exact `Scope:` line (own line, this literal wording) on every entry — downstream scheduling keys off it verbatim, so free prose like \"this is a stretch goal\" instead of the tag will not be recognised. Tag an entry `deferred` when it is explicitly out of scope for v1, a stretch goal, or something you are deliberately not proposing for immediate implementation; follow the tag with a short inline reason (e.g. `Scope: deferred (future / not a v1 blocker) — needs the batch API landing in phase 2`).\n");
    out.push_str("  - **size each entry to one reviewable PR by one worker in one session.** This is the granularity scheduling materialises into tasks, so pre-split the work here — an oversize entry forces the scheduler to reject and re-plan it:\n");
    out.push_str("    - keep each entry single-subsystem and single-PR. Scope that spans several subsystems (engine + cli + protocol + app + …) is several entries with dependency edges, not one.\n");
    out.push_str("    - multi-phase scope (\"parse (i)… and (ii)… and emit… and validate…\") is several entries — list each phase separately with explicit dependencies, never one entry that does it all.\n");
    out.push_str("    - sweeps and validation campaigns (\"validate/sweep/migrate all N X\", an all-lists reconciliation, a corpus-wide fixture sweep) are separate dependent entries, listed after the implementation they validate — do not fold them into the implementer.\n");
    out.push_str("    - unknown-format discovery (study / dump / reverse-engineer / reconcile-against-source) is its own investigation entry, sequenced before the implementation that consumes its findings.\n");
    out.push_str("    - if an entry needs a paragraph to describe, it is probably several tasks — split it.\n");
    out.push_str("  - note which tasks at the same dependency depth may run in parallel, so the task graph (not just a linear list) is expressible.\n");
    out.push_str("  - when you mark tasks parallel, weigh **file** overlap, not just functional independence: two tasks can be independent in design yet edit the same file (e.g. a compact-view task and a detail-view task that both edit the same component/container). If two otherwise-parallel tasks are clearly and substantially likely to co-edit the same files, say so — give them a defined order and note that the later one must forward-port the earlier one's changes preservingly (integrate, never delete). Do not over-serialise: only flag clear, substantial overlap; incidental overlap stays parallel.\n");
    out.push_str("  - include items that are deferred or explicitly out of scope as their own entries (tagged `Scope: deferred (future / not a v1 blocker)`, see above) rather than silently omitting them — silent omissions force the coordinator to guess what was considered and rejected. Do not drop the entry just because it is deferred; the scope tag is what lets it stay visible without being auto-started.\n");
    out.push_str("  - This section is what the design doc's auto-populate step will consume to materialise dependent tasks with edges, so completeness matters.\n");
    out.push_str(&design_questions_manifest_block());
    out.push_str("- when the doc is ready for review, push it and open a PR (see the acceptance criterion below). Do not start implementation tasks — those come from follow-up work items the human files after the design is approved.\n");
    out
}

/// Directive block for a `kind = 'design_postmortem'` task, auto-scheduled
/// by `project_postmortem_sweep` once a project's implementation work
/// drains to zero. Deliberately the mirror image of
/// [`compose_design_directive`]: that one says "author a new doc"; this one
/// says "reconcile the existing doc against what actually shipped." The
/// task's own `description` (rendered above this block via
/// `work_item_details`) already carries the remit brief — the project's
/// design-doc path/branch and the enumerated merged PRs to review — so this
/// block only needs to state the doc-only scope constraint and the update
/// method.
fn compose_design_postmortem_directive(parent_project: Option<&Project>, structured_output_path: &str) -> String {
    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str(
        "- the deliverable is an **update to the project's existing design document**, not a new document and not an implementation. Do not edit code, do not start prototyping, do not open partial implementation PRs.\n",
    );
    out.push_str(
        "- the PR for this run contains **only the design doc update** (edits to the existing markdown file). If you find yourself touching `.rs`, `.ts`, `.swift`, build files, or anything else, stop — you are out of scope.\n",
    );
    if let Some(path_line) = canonical_design_doc_path_line(parent_project) {
        out.push_str(&path_line);
    }
    out.push_str(&doc_structure_conventions_block());
    out.push_str("- review each merged PR listed in the details above (`gh pr view`/`gh pr diff`) alongside the current doc, and update the doc to reflect **as-built reality**:\n");
    out.push_str("  - decisions that diverged from what the doc originally said, and why (as best you can tell from the PR/commit history).\n");
    out.push_str("  - scope that was added or dropped relative to the doc's plan.\n");
    out.push_str("  - contracts, interfaces, or data models that evolved during implementation.\n");
    out.push_str(
        "- edit the doc in place — do not append a separate \"postmortem\" or \"changelog\" section unless the doc already uses that structure. The goal is a design doc that reads as if it were written *after* the work, not a diff log bolted onto the original.\n",
    );
    out.push_str(
        "- if the merged PRs matched the doc's plan closely, the update may be small (e.g. a note confirming what shipped matches the design) — that's fine, but still open a PR with that update rather than stopping with no PR at all.\n",
    );
    out.push_str("- open a PR with the update regardless of which repo the doc lands in — the PR is the review window, same as any design change.\n");
    out.push_str(&postmortem_followups_emission_block(structured_output_path));
    out
}

/// Required (not optional) structured-output instruction for
/// `design_postmortem` tasks: uncompleted work the review surfaces —
/// scope claimed but not delivered, a handoff that fell through (e.g. a
/// wire field shipped backend-side whose frontend consumption was never
/// done), or work the design promised that no task ever owned — must
/// become real follow-up tasks, not just a mention in the doc. A prior
/// engine feature found that free-text "filed as a follow-up" claims in
/// worker PR bodies had a 100% miss rate because no write path for them
/// ever existed; this artifact IS that write path, so it is mandatory: the
/// engine (`postmortem_followups::reconcile_postmortem_followups`) treats
/// a missing file as an error, not as "found nothing."
fn postmortem_followups_emission_block(output_path: &str) -> String {
    let mut out = String::new();
    out.push_str("\n## Required: report uncompleted work surfaced by this review\n\n");
    out.push_str(
        "While reviewing the project's PRs against the design doc, you may find work the design promised but that no task ever delivered — scope a task claimed but didn't finish, a handoff that fell through (e.g. a backend field shipped with no frontend consumption), or a gap between plan and as-built reality that needs its own follow-up task. This is DIFFERENT from documenting what shipped in the doc itself: these are gaps that need NEW work scheduled.\n\n",
    );
    out.push_str(&format!(
        "You MUST **write** a JSON array to this exact file (also exported as `$BOSS_STRUCTURED_OUTPUT`) before finishing, even if the array is empty:\n\n`{output_path}`\n\nThis path is outside the repo/workspace, so it never pollutes your PR. Omitting the file entirely is treated by the engine as an error, not as \"no findings\" — writing `[]` is how you report that you found no uncompleted work. Each array element is an object with all three fields **required**:\n",
    ));
    out.push_str("- `name` (required): a short, specific task title.\n");
    out.push_str("- `description` (required): what the task needs to deliver.\n");
    out.push_str(
        "- `evidence` (required): the concrete signal that this work is genuinely missing — a PR number, a code/doc reference, a specific gap you observed. Not a vague impression.\n\n",
    );
    out.push_str(
        "File contents example (non-empty):\n\n```json\n[{\"name\": \"Wire the frontend to the new export field\", \"description\": \"PR #142 added `export_format` to the API response but no UI consumes it.\", \"evidence\": \"grep for export_format in app-macos/Sources shows zero references; design doc \\u00a74 calls for a format picker.\"}]\n```\n\n",
    );
    out.push_str("File contents example (nothing found):\n\n```json\n[]\n```\n\n");
    out.push_str(
        "The engine creates a real task in this project for every entry — these are NOT proposals a human reviews first, so only include genuine gaps you have concrete evidence for, never speculative or restated-scope items.\n",
    );
    out
}

/// Attentions question-manifest emission instruction (design:
/// `tools/boss/docs/designs/attentions.md`, "Creation pipeline"). Appended
/// to the `project_design` directive: a design worker that has genuine open
/// questions for the human emits a sibling `<slug>.attentions.json` manifest
/// next to the doc. The engine's `DesignDetector` parses it off the PR
/// branch and upserts an inline question group the human answers in the doc
/// viewer, batched into a single revision.
fn design_questions_manifest_block() -> String {
    let mut out = String::new();
    out.push_str("- OPTIONAL — open questions for the human: if, while writing the doc, you have specific decisions you want a human to make (yes/no calls, multiple-choice forks, or free-text prompts), emit a **questions manifest** as a sibling file next to the design doc — the same path with the `.md` extension replaced by `.attentions.json` (e.g. `…/designs/<slug>.attentions.json`).\n");
    out.push_str("  - The file is a JSON array. Each entry is an object:\n");
    out.push_str("    - `question_type` (required): one of `yes_no` | `multiple_choice` | `prompt` (free text).\n");
    out.push_str("    - `prompt` (required): the question shown to the human.\n");
    out.push_str("    - `choices` (required only for `multiple_choice`): a JSON array of option strings.\n");
    out.push_str("    - `anchor` (optional but encouraged): the heading slug the question is about, so it renders next to the relevant section.\n");
    out.push_str("  - Example: `[{\"question_type\":\"yes_no\",\"prompt\":\"Gate extraction behind a flag?\",\"anchor\":\"rollout\"},{\"question_type\":\"multiple_choice\",\"prompt\":\"One table or two?\",\"choices\":[\"one\",\"two\"],\"anchor\":\"data-model\"}]`\n");
    out.push_str("  - Only emit this when you genuinely need the human to decide something; omit the file entirely otherwise. Do NOT restate the doc's \"Risks / open questions\" prose here — the manifest is just the machine-actionable subset you want answered. The engine batches all entries into one group, so answering them yields a single doc revision.\n");
    out
}

/// Followups emission instruction (design:
/// `tools/boss/docs/designs/attentions.md`, "Creation pipeline"). Appended to
/// the implementation-worker directive: a worker that notices concrete,
/// out-of-scope follow-on work near task completion **writes** it as a JSON
/// array to the engine-owned artifact at `output_path` (see
/// [`crate::structured_output`]). The engine reads + schema-validates that
/// file at completion and upserts a followup group keyed to this task; the
/// human turns accepted entries into tasks with one gesture. A `FOLLOWUPS:`
/// fenced-JSON sentinel in the final message is kept as a transitional
/// fallback (and to keep remote workers working until the artifact is fetched
/// cross-host).
fn followups_emission_block(output_path: &str) -> String {
    let mut out = String::new();
    out.push_str("\n## Optional: surface follow-on work as followups\n\n");
    out.push_str(
        "If, while completing this task, you noticed concrete follow-on work worth filing — a separate bug, a needed refactor, a missing test, a docs gap — that is OUT OF SCOPE for this PR, you may surface it for the human. This is OPTIONAL: only include genuine, actionable proposals, never invent work to fill it, and never list the change you just made.\n\n",
    );
    out.push_str(&format!(
        "If (and only if) you have followups, **write** a JSON array of them with the `Write` tool to this exact file (also exported as `$BOSS_STRUCTURED_OUTPUT`):\n\n`{output_path}`\n\nThis path is outside the repo/workspace, so the manifest never pollutes your PR. Each array element is an object:\n",
    ));
    out.push_str("- `proposed_name` (required): a short task title.\n");
    out.push_str("- `proposed_description` (required): one paragraph of scope.\n");
    out.push_str("- `proposed_effort` (optional): one of `trivial` | `small` | `medium` | `large` | `max`.\n");
    out.push_str("- `proposed_work_kind` (optional): one of `task` | `chore` | `project` (defaults to `task`).\n");
    out.push_str("- `rationale` (optional): why it is worth doing.\n\n");
    out.push_str("File contents example:\n\n```json\n[{\"proposed_name\": \"Add retry/backoff to the X client\", \"proposed_description\": \"The X client fails hard on transient 5xx; add bounded retry with jitter.\", \"proposed_effort\": \"small\", \"proposed_work_kind\": \"task\", \"rationale\": \"Observed flakes during this task.\"}]\n```\n\n");
    out.push_str("Do NOT write the file at all if you have no followups — an absent file means \"no followups\", which is the normal case. Writing it does not block this PR — it just files proposals for the human to review.\n\n");
    out.push_str("As a fallback only (e.g. if the file write is unavailable), you may instead append — after your `## Open Questions` section — a line containing exactly `FOLLOWUPS:` immediately followed by a fenced ```json code block holding the same JSON array.\n");
    out
}

/// Directive block for `kind = 'investigation'` tasks. States the
/// deliverable shape (one markdown doc, PR only, no code) and the
/// repo routing rules so the worker doesn't need to infer them.
///
/// Key divergence from design tasks:
/// - Destination repo is the product's `docs_repo` (or
///   `BOSS_USER_DOCS_REPO`) — NOT the product's code repo.
/// - No section template: free-form markdown. The investigation brief
///   drives the structure.
/// - PR is mandatory even on the user's personal docs repo. The
///   direct-push shortcut in the user's CLAUDE.md does NOT apply here:
///   the PR review window is the user's opportunity to edit the doc
///   before it is saved for posterity. Always open a PR.
///
/// The kanban doc affordance is derived from the task's `pr_url`, which
/// the engine auto-detects when the worker opens the PR — exactly like a
/// design task. The worker does NOT register any doc pointer; opening the
/// PR is the whole job.
fn compose_investigation_directive() -> String {
    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str("- the deliverable is a **markdown document**, not code. Do not edit source code, build files, or anything other than the investigation doc.\n");
    out.push_str("- the PR for this run contains **only the markdown doc** (one new file). If you find yourself touching `.rs`, `.ts`, `.swift`, build files, or anything else, stop — you are out of scope.\n");
    out.push_str("- choose a filename that reflects the topic (e.g. `docs/investigations/my-topic.md`). Use an `investigations/` subdirectory if one exists in the repo, or create it.\n");
    out.push_str(&doc_structure_conventions_block());
    out.push_str("- open a PR with the doc regardless of which repo it lands in. Do NOT push directly to `main` even on the user's personal docs repo (e.g. `brianduff/docs`). The PR is the user's edit window. The kanban card's doc link is derived from this PR automatically — there is no separate pointer to register.\n");
    out.push_str("- investigations do not touch code. If the description asks for both research and a code change, write only the investigation doc and note the follow-up code changes at the end of the doc for the user to file separately.\n");
    out
}

/// Compose the initial prompt for an `answer_agent` execution (P3b of
/// `comment-triggered-document-revisions.md`). `execution.work_item_id` is
/// the comment id (see `WorkDb::create_answer_agent_execution`); this
/// resolves it back to the comment, its doc owner, the doc's full content
/// (fetched via `gh api` at the doc's own branch/ref — the leased workspace
/// checkout is at whatever default ref cube gave it, not necessarily this
/// branch, so the doc text is embedded directly rather than read from disk;
/// see the answer-agent capability table's "read code in a leased checkout"
/// vs. "read the commented-on document" distinction), and any prior thread
/// entries (non-empty on a `thread_turn > 0` re-entered follow-up, phase 3c).
///
/// Falls back to the generic implementer prompt — logging a warning — if the
/// comment or its doc owner can no longer be resolved (raced/deleted
/// mid-flight), mirroring the triage/reviewer fallback pattern above: a
/// weaker prompt is better than no spawn at all.
pub(super) async fn compose_answer_agent_prompt(work_db: &WorkDb, execution: &WorkExecution) -> String {
    let comment_id = &execution.work_item_id;
    let fallback = |reason: &str| -> String {
        tracing::warn!(
            execution_id = %execution.id,
            comment_id = %comment_id,
            reason,
            "answer_agent execution: could not compose the answer-agent prompt; \
             falling back to a minimal generic prompt",
        );
        format!(
            "You are a read-only answer agent (see your CLAUDE.md for the full \
             read-only mandate). The engine could not resolve the comment this run was \
             spawned for ({reason}). Post a single reply via `{cmd}` explaining that you \
             were unable to load the question, then stop.",
            cmd = crate::answer_agent::THREAD_REPLY_COMMAND,
        )
    };

    let comment = match work_db.get_comment(comment_id) {
        Ok(Some(c)) => c,
        Ok(None) => return fallback("comment not found"),
        Err(err) => return fallback(&format!("failed to load comment: {err}")),
    };
    let doc_owner = match work_db.resolve_doc_owner(&comment.artifact_kind, &comment.artifact_id) {
        Ok(Some(owner)) => owner,
        Ok(None) => return fallback("comment's artifact has no design/investigation doc owner"),
        Err(err) => return fallback(&format!("resolve_doc_owner failed: {err}")),
    };

    let doc_content = match parse_pr_doc_artifact_id(&comment.artifact_id) {
        Some((repo, branch, path)) => match boss_design_doc_fetcher::fetch_design_doc(&repo, &path, &branch).await {
            boss_design_doc_fetcher::DocFetchOutcome::Content(text) => Some((path, text)),
            boss_design_doc_fetcher::DocFetchOutcome::DocMissing => {
                tracing::warn!(
                    execution_id = %execution.id,
                    comment_id = %comment_id,
                    repo, branch, path,
                    "answer_agent execution: doc no longer exists at this ref; \
                     the agent will answer from the comment's anchor context alone",
                );
                None
            }
            boss_design_doc_fetcher::DocFetchOutcome::FetchFailed { reason } => {
                tracing::warn!(
                    execution_id = %execution.id,
                    comment_id = %comment_id,
                    repo, branch, path, reason,
                    "answer_agent execution: doc fetch failed; \
                     the agent will answer from the comment's anchor context alone",
                );
                None
            }
        },
        // Only `pr_doc` artifacts reach here (`resolve_doc_owner` scopes to
        // that kind), so this is unreachable in practice; degrade gracefully.
        None => None,
    };

    let thread = work_db.list_comment_thread_entries(comment_id).unwrap_or_default();

    let mut prompt = String::new();
    prompt.push_str(
        "You are a read-only \"mini-coordinator\" answer agent, spawned to answer one \
         reviewer question left as a comment on a design/investigation document. Your \
         CLAUDE.md states the full read-only mandate and the one command you may run to \
         reply — read it before doing anything else.\n\n",
    );
    prompt.push_str(&format!(
        "## The question\n\n\
         Document: `{path}` (task {task_id}, `{task_kind}`)\n\n",
        path = doc_content
            .as_ref()
            .map(|(p, _)| p.as_str())
            .unwrap_or(comment.artifact_id.as_str()),
        task_id = doc_owner.task_id,
        task_kind = doc_owner.task_kind,
    ));
    prompt.push_str(&format!(
        "Quoted section (the highlighted span, with surrounding context):\n> {prefix}[[{exact}]]{suffix}\n\n",
        prefix = comment.anchor.prefix,
        exact = comment.anchor.exact,
        suffix = comment.anchor.suffix,
    ));
    prompt.push_str(&format!("Comment:\n> {body}\n\n", body = comment.body));

    if !thread.is_empty() {
        prompt.push_str("## Prior thread on this comment\n\n");
        for entry in &thread {
            prompt.push_str(&format!(
                "**{}** ({}):\n{}\n\n",
                entry.entry_kind, entry.author, entry.body
            ));
        }
    }

    match &doc_content {
        Some((_, text)) => {
            prompt.push_str("## Full document content\n\n");
            prompt.push_str("```markdown\n");
            prompt.push_str(text);
            prompt.push_str("\n```\n\n");
        }
        None => {
            prompt.push_str(
                "## Full document content\n\n\
                 Not available (fetch failed or the doc no longer exists at this ref) — \
                 answer from the quoted section above, and use your leased workspace / \
                 read-only tools if you need more context.\n\n",
            );
        }
    }

    prompt.push_str(&format!(
        "## Your task\n\n\
         Answer the question above as thoroughly and accurately as you can. You may read \
         anything the Boss coordinator can see and read code in your leased workspace, but \
         you may not edit, push, or mutate any state. When you have a complete answer, post \
         it with:\n\n\
         ```\n{cmd} --body \"<your comprehensive answer>\"\n```\n\n\
         Post exactly one reply, then stop. Your answer may include a concrete proposed edit \
         as a prose sketch, but you have no mechanism to apply it — do not attempt to.\n",
        cmd = crate::answer_agent::THREAD_REPLY_COMMAND,
    ));

    prompt
}

/// Directive block for `kind = 'revision'` tasks.
///
/// A revision's deliverable is a NEW COMMIT on an EXISTING pull request —
/// the PR owned by the parent task's chain root.  The revision worker must
/// NOT open a new PR.  The parent's PR URL is carried in
/// `execution.pr_url` (set at dispatch time).
///
/// When `conflict_attempt` or `ci_attempt` is `Some`, a signal-specific
/// diagnostic fragment is appended (design Q3 of
/// `unify-pr-remediation-on-revisions.md`): the existing diagnosis/log
/// rendering from the bespoke composers is lifted into the shared revision
/// directive rather than duplicated across three nearly-identical prompts.
fn compose_revision_directive(
    execution: &crate::work::WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    conflict_attempt: Option<&ConflictResolution>,
    ci_attempt: Option<&CiRemediation>,
    merge_order_preservation: &[String],
    worker_signal_proposals_seam_enabled: bool,
) -> String {
    let description = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task.description.trim().to_owned(),
        _ => String::new(),
    };
    let parent_pr_url = execution.pr_url.as_deref().unwrap_or("(unknown)");
    let pr_number = boss_github::pr_url::pr_number_from_url(parent_pr_url)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".into());
    let repo_slug =
        crate::completion::parse_repo_slug(&execution.repo_remote_url).unwrap_or_else(|_| "<owner/repo>".to_owned());
    // A conflict-resolution revision pushes the merge-corrected branch as
    // soon as it COMPILES (the merge-correctness gate); the PR's own CI
    // runs the full test suite post-push. Other revisions keep the
    // build-and-test-before-push gate.
    let is_conflict_resolution = conflict_attempt.is_some();

    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str("- This is a **REVISION** task. Your deliverable is an update to an EXISTING pull request — typically a new commit on the PR branch, or a rebase if that is all that is needed. Do NOT open a new PR. Do NOT create a `boss/exec_*` bookmark.\n");
    out.push_str(&format!("- The parent PR is #{pr_number} at {parent_pr_url}.\n"));
    out.push_str(&format!("- What this revision should change: {description}\n"));
    out.push_str(&format!(
        "\n**`gh` requires `--repo` in this workspace:** This repo is `{repo_slug}`. \
         `gh` cannot auto-detect the repo in a jj workspace (there is no `.git` \
         directory at the root — only `.jj/`). Pass `--repo {repo_slug}` on every \
         `gh` command: `gh pr view`, `gh pr checks`, `gh pr diff`, `gh api`, etc.\n"
    ));
    // Issue #804: revision chores on PR #250 were the worst
    // offenders for pushing red code. Apply the pre-push build gate when
    // the workspace is a Bazel workspace. Conflict-resolution revisions
    // get the merge-correctness variant (build before push; tests run in
    // the PR's CI after the push) so a correct resolution is never
    // stranded behind a slow/flaky full-suite run.
    let prepush_gate = if is_conflict_resolution {
        bazel_conflict_resolution_gate_block(workspace_path, worker_signal_proposals_seam_enabled)
    } else {
        bazel_prepush_gate_block(workspace_path, worker_signal_proposals_seam_enabled)
    };
    if let Some(gate) = prepush_gate {
        out.push_str(&gate);
    }
    out.push('\n');
    out.push_str("## Workspace state\n");
    // `pr_number != "?"` is equivalent to `execution.pr_url` being a parseable
    // GitHub PR URL, which is exactly when the engine called `cube workspace goto`
    // to position the workspace at the PR head. Without a parseable URL,
    // the workspace is on main and the worker must position it manually.
    if pr_number != "?" {
        if is_conflict_resolution {
            out.push_str("The engine pre-positioned this workspace via `cube workspace goto`, so you are already on a fresh editable commit whose parent is the PR head — no branch discovery or checkout is needed. Do NOT start making changes yet: this is a conflict-resolution revision, and the ground-truth block below requires you to check GitHub's mergeable status and re-run the rebase FIRST.\n");
        } else {
            out.push_str("The engine pre-positioned this workspace via `cube workspace goto`, so you are already on a fresh editable commit whose parent is the PR head. Start making your changes directly — no branch discovery or checkout is needed.\n");
        }
        out.push('\n');
        out.push_str(
            "**Fallback** (only if the workspace is NOT already positioned on an editable change atop the PR head):\n",
        );
        out.push_str("```\n");
        out.push_str(&format!("cube workspace goto --pr {pr_number}\n"));
        out.push_str("```\n");
    } else {
        out.push_str(
            "**The engine could not determine the PR number from the pr_url field. \
             You MUST position the workspace manually before making any changes \
             (replace `<n>` with the actual PR number):**\n",
        );
        out.push_str("```\n");
        out.push_str("cube workspace goto --pr <n>\n");
        out.push_str("```\n");
    }
    out.push_str("IMPORTANT: NEVER run `jj edit`, `gh pr checkout`, or `git checkout` in this workspace — fetched remote commits are immutable and those tools do not work correctly in a jj workspace.\n");
    out.push('\n');
    out.push_str("Steps:\n");
    out.push_str("1. Make the requested change.\n");
    out.push_str("2. `jj describe -m \"<short message describing THIS revision's change>\"`\n");
    out.push_str("3. Find the parent bookmark name and advance it to the new commit:\n");
    out.push_str("   ```\n");
    out.push_str("   # Find the parent bookmark (strip the @origin suffix for the branch name):\n");
    out.push_str("   jj log -r 'parents(@)' --no-graph -T 'remote_bookmarks'\n");
    out.push_str("   # Advance the local bookmark:\n");
    out.push_str("   jj bookmark set <parent-branch-name> -r @\n");
    out.push_str("   ```\n");
    out.push_str(
        "4. `cube pr update --branch <parent-branch-name>`   # pushes to the existing PR; no GIT_DIR or --allow-new needed.\n",
    );
    out.push_str("5. **Update the PR title AND description** — this is a required step, not optional:\n");
    out.push_str(&format!(
        "   a. Read the current title and description: `gh pr view {pr_number} -R {repo_slug} --json title,body -q '\"title: \" + .title + \"\\n\\n\" + .body'`\n"
    ));
    out.push_str("   b. Compare the title and description carefully against what the PR NOW does after your change. Pay special attention to any section that describes behaviour, scope, or approach that this revision REVERSES, supersedes, or obsoletes — those sections MUST be corrected or removed. A description that tells a reviewer the exact opposite of what the code does is worse than a terse one.\n");
    out.push_str(&format!("   b2. **PR title — check it explicitly.** If the revision changes or overturns the PR's scope or conclusion (e.g. the original PR claimed something was not a bug but this revision fixes the bug), the title MUST be updated to reflect the final state. A title that contradicts the committed code is a defect. Update it with: `gh pr edit {pr_number} --title \"<accurate new title>\" -R {repo_slug}`\n"));
    out.push_str("   c. If any part of the description is now inaccurate, write the corrected body to a temp file and apply it:\n");
    out.push_str(&format!(
        "      `body=$(mktemp) && <write corrected body to $body> && gh pr edit {pr_number} --body-file \"$body\" -R {repo_slug}`\n"
    ));
    out.push_str(
        "      Never pass the body as an inline `--body` argument — the shell evaluates backticks and `$(...)`.\n",
    );
    out.push_str("   d. What to write: rewrite the description so it is accurate and self-contained for reviewers NOW. The main summary must describe the CURRENT state — what the PR does, not what it used to do. Do NOT append a changelog that leaves a contradictory original summary above it; instead correct the summary in place. A brief \"Changes in this revision\" note may follow the corrected summary if it adds context, but it must never contradict or overshadow the corrected summary.\n");
    out.push_str("   e. A revision may skip steps c–d ONLY if it changes ZERO source files (e.g. a PR-description-only fix or a pure markdown/comment edit) AND involves no rebase, merge, or conflict resolution. Rebase and conflict-resolution revisions do NOT qualify for this skip — they touch compiled output and must go through the full description review. The title check (step b2) is NEVER skippable — always verify it.\n");
    out.push('\n');
    out.push_str(&format!(
        "6. Confirm the new commit is on the PR: `gh pr view {pr_number} -R {repo_slug}`\n"
    ));
    out.push_str(&format!(
        "7. Print the parent PR URL on its own line as the FINAL thing in your final response: {parent_pr_url}\n"
    ));
    out.push('\n');
    out.push_str("Preserve revision history — each revision is a new commit on the PR branch; never amend, squash, or rename existing commits on the branch.\n");
    out.push('\n');
    let rebase_gate_clause = if is_conflict_resolution {
        "Rebase-only exception (VCS only — not a build-gate skip): if the ONLY thing needed to satisfy this revision is a rebase (e.g. rebasing the branch onto updated main) and the rebase produces NO diff whatsoever (zero changed files), it is valid to have NO new commit. Do not manufacture an empty or cosmetic commit. In that case, push the rebased branch and explain in your response that the revision was satisfied by a rebase with no code change. IMPORTANT: this exception covers VCS mechanics only — whether to add a new commit. It does NOT exempt you from the merge-correctness build gate. Any rebase, merge, or conflict resolution MUST run the `bazel build` merge-correctness gate (compile the touched/upstream targets, regenerate invalidated lockfiles) before pushing, even when the rebase appeared clean — a rebase merges upstream changes in and the resulting code is new and must compile. The full `bazel test` suite is NOT a precondition for this push; it runs in the PR's CI after you push (see the conflict-resolution gate above).\n"
    } else {
        "Rebase-only exception (VCS only — not a build-gate skip): if the ONLY thing needed to satisfy this revision is a rebase (e.g. rebasing the branch onto updated main) and the rebase produces NO diff whatsoever (zero changed files), it is valid to have NO new commit. Do not manufacture an empty or cosmetic commit. In that case, push the rebased branch and explain in your response that the revision was satisfied by a rebase with no code change. IMPORTANT: this exception covers VCS mechanics only — whether to add a new commit. It does NOT exempt you from the pre-push build gate. Any revision that involves a rebase, merge, or conflict resolution MUST run the full `bazel build` + `bazel test` gate before pushing, even when the rebase appeared clean. A rebase merges upstream changes into your branch — the resulting code is new and must be compiled and tested. This is exactly where compile errors get reintroduced.\n"
    };
    out.push_str(rebase_gate_clause);
    out.push('\n');
    out.push_str("Constraints:\n");
    out.push_str("- Do NOT run `gh pr create` — this revision has no PR of its own.\n");
    out.push_str("- Do NOT create a `boss/exec_*` bookmark — push to the existing parent branch.\n");
    out.push_str("- Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty and this is NOT a rebase-only revision, stop and explain.\n");
    out.push('\n');
    out.push_str(check_bypass_prohibition_text());
    out.push('\n');
    out.push_str(&deferred_scope_directive());
    out.push_str(&format!(
        "\nAcceptance criterion: when you believe the work is done, the deliverable is the parent PR URL.\n\
         - Push your changes to the parent branch (see step 4 above). Do NOT open a new PR.\n\
         - Update the PR title and description per step 5 above — a stale or contradictory title or description is a defect. If this revision changes or overturns the PR's scope or conclusion, the title MUST reflect the final state.\n\
         - Confirm the parent PR shows your new commit with `gh pr view {pr_number} -R {repo_slug}`.\n\
         - Print {parent_pr_url} on its own line as the final thing in your final response so the engine can pick it up.\n\
         - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty and no rebase was needed, stop and explain.\n"
    ));
    if let Some(attempt) = conflict_attempt {
        out.push_str(&compose_conflict_resolution_fragment(attempt));
        out.push_str(&compose_merge_order_preservation_fragment(merge_order_preservation));
    }
    if let Some(attempt) = ci_attempt {
        out.push_str(&compose_ci_remediation_fragment(attempt));
    }
    out
}

/// Render one human-facing line per already-merged `merge_order` sibling,
/// naming the task and (when known) the PR whose surfaces the forward-port
/// must preserve.
pub(super) fn render_merge_order_preservation_lines(
    siblings: &[crate::work_dependencies::MergeOrderMergedSibling],
) -> Vec<String> {
    siblings
        .iter()
        .map(|s| match &s.pr_url {
            Some(url) if !url.is_empty() => format!("`{}` (merged: {url})", s.task_id),
            _ => format!("`{}`", s.task_id),
        })
        .collect()
}

/// Sibling-specific preservation clause for a forward-port conflict brief
/// (merge_order sequencing, direction 2). When the conflict revision's parent
/// has a `merge_order` sibling that already merged, the base moved *because*
/// that overlap partner landed — so this resolution is exactly the incident-002
/// forward-port hazard. Name the merged sibling(s) explicitly so the worker
/// knows precisely which merged work to preserve; the both-parents deletion
/// tripwire ([`crate::merge_parent_deletion`]) verifies the result regardless.
///
/// Empty `merged_siblings` ⇒ empty string (no overlap partner merged; the
/// generic preservation rule already present in the conflict fragment stands).
fn compose_merge_order_preservation_fragment(merged_siblings: &[String]) -> String {
    if merged_siblings.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("\n### Merge-order preservation contract (sibling overlap — CRITICAL)\n\n");
    out.push_str(
        "This PR was flagged at planning time as editing files that overlap with a sibling \
         task, and that sibling has now **merged first** — which is exactly why your base \
         moved. This is the incident-002 forward-port hazard: the conflict exists *because* \
         merged work landed on your base. You MUST integrate (never delete) the following \
         merged sibling's surfaces:\n\n",
    );
    for sib in merged_siblings {
        out.push_str(&format!("- {sib}\n"));
    }
    out.push_str(
        "\nDeleting any surface these siblings added — to make the conflict disappear — is a \
         defect, not a resolution. If you believe a surface is genuinely superseded, STOP and \
         escalate per the preservation rule above (cite the design doc; do not push a deletion). \
         The engine's both-parents deletion tripwire diffs your resolution against the merged \
         base and will halt auto-progression on any merged-parent surface you remove.\n\n",
    );
    out
}

/// If the parent project has an explicit `design_doc_path` pointer
/// (set via `boss project design-doc`), emit that as the canonical
/// path. Otherwise fall back to the `<repo>/docs/designs/<slug>.md`
/// convention, anchored on the project's slug so two design tasks
/// don't collide. Returns `None` only when we have no project at
/// all — in practice the dispatcher always has one for
/// `kind = 'design'` rows, but the runner stays defensive.
fn canonical_design_doc_path_line(parent_project: Option<&Project>) -> Option<String> {
    let project = parent_project?;
    if let Some(path) = project
        .design_doc_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        return Some(format!(
            "- the canonical path for this design doc is `{path}` (set on the project's `design_doc_path` pointer). Write the doc there.\n",
        ));
    }
    let slug = if project.slug.trim().is_empty() {
        "design"
    } else {
        project.slug.trim()
    };
    Some(format!(
        "- the project's `design_doc_path` pointer is not yet set. Place the doc at `docs/designs/{slug}.md` (the repo's convention; adjust to the product's docs layout if the repo already has one — e.g. `tools/boss/docs/designs/{slug}.md` for the Boss product). After you create the file, set the pointer with `boss project set-design-doc --project <id> --path <path>` so the next run resolves it directly.\n",
    ))
}

/// Signal-specific fragment appended to `compose_revision_directive` when the
/// revision was created with `created_via = "merge-conflict:<crz_id>"`.
///
/// Provides the conflict context and diagnosis that the worker needs to
/// resolve the merge conflict — identical in content to the bespoke
/// `compose_conflict_resolution_prompt` except that the branch/push spine
/// is already covered by the shared revision directive, so this fragment
/// covers only the signal-specific parts: the diagnosis block, rebase
/// instructions, stop conditions, and post-resolution PR comment template.
fn compose_conflict_resolution_fragment(attempt: &ConflictResolution) -> String {
    let mut out = String::new();
    out.push_str("\n---\n\n");
    out.push_str(&format!(
        "## Conflict resolution context: PR #{pr_num} against `{base}`\n\n",
        pr_num = attempt.pr_number,
        base = attempt.base_branch,
    ));
    out.push_str(&format!(
        "**Branch**: `{}` based off `{}`\n",
        attempt.head_branch, attempt.base_branch,
    ));
    if let Some(base_sha) = attempt.base_sha_at_trigger.as_deref() {
        out.push_str(&format!(
            "**Base sha at conflict detection**: `{base_sha}` (current `{}` may be ahead)\n",
            attempt.base_branch,
        ));
    }
    out.push_str(&format!("**Attempt id**: `{}`\n\n", attempt.id));
    out.push_str(
        "This PR was in code review when `main` moved under it. The PR's diff against\n\
         the current `main` does not apply cleanly. Your task in step 3 above is to\n\
         resolve the conflicts — **you are not adding new work to this PR.**\n\n",
    );
    out.push_str(&compose_conflict_ground_truth_fragment(attempt));
    out.push_str(
        "### Preservation rule (HARD CONSTRAINT — read before resolving)\n\n\
         A merge/forward-port resolution is a **reconciliation**, not an authoring surface. \
         Its only correct default is **preserve both sides**:\n\n\
         - **A resolution must NOT remove functionality introduced by either parent.** If \
         both `main` and this PR added a feature that now overlaps, integrate them — do not \
         drop one side to make the conflict disappear. Deleting the harder-to-merge side is \
         never the default resolution.\n\
         - **If you believe one side is genuinely superseded, STOP.** Do not delete it and \
         rationalize the removal. Deletion of code a merged parent added is an operator \
         decision, not a resolution choice: run `boss engine conflicts mark-failed <attempt-id> \
         --reason product_decision_required`, comment on the PR explaining the situation, and \
         do NOT push a resolution that drops the feature.\n\
         - **Any removal of code a merged parent added must be called out explicitly** in your \
         PR comment and PR description (see the Removed section in the comment template below) \
         AND justified with a **specific design-doc citation** (path + section) that authorizes \
         the removal. \"It looks superseded\", \"it's now orphaned/dead\", or a clean `tsc`/build \
         is NOT a justification — a component is only \"orphaned\" if something other than this \
         very resolution orphaned it. Absent a design-doc citation that says one surface \
         replaces the other, both surfaces must survive.\n\n",
    );
    out.push_str("### Rebase steps (replaces step 3)\n\n");
    out.push_str(
        "Run the cube rebase command — it encodes the correct jj recipe automatically \
         and avoids the `@origin` / immutable-heads pitfalls agents commonly hit:\n\n\
         ```\n\
         cube workspace rebase\n\
         ```\n\n\
         This command: fetches the latest integration branch from GitHub, resolves this \
         workspace's boss branch automatically (no branch name argument needed), rebases \
         it onto the repo's configured integration branch with `--ignore-immutable`, and \
         reports a clear signal:\n\n\
         - `REBASED_CLEAN` — no conflicts; the branch has been pushed automatically. Skip to step 5 (update PR description).\n\
         - `REBASED_WITH_CONFLICTS` — conflicts are materialized in the working copy. \
         Inspect with `jj st` and `jj resolve --list`, read the diagnosis below for what \
         was touched on the upstream side, resolve each file, then continue to step 4.\n\n\
         Do NOT hand-roll `jj rebase` manually — the correct flags differ from the bare \
         form and agents reliably get them wrong.\n\n",
    );
    out.push_str(
        "### How to resolve jj conflicts (first-class conflicts — stacked branches)\n\n\
         **jj records conflicts IN each commit independently.** `jj git push` refuses to push \
         ANY commit that still contains a conflict, including ancestors. Resolving only the \
         working-copy tip does NOT clear conflicts baked into parent commits — this is the most \
         common failure mode on stacked branches.\n\n\
         **Step A — List every conflicted commit on the branch:**\n\
         ```\n\
         jj log -r '::<branch>' -T 'change_id ++ \" \" ++ description.first_line() ++ \" conflicts=\" ++ conflict ++ \"\\n\"'\n\
         ```\n\
         Note every commit with `conflicts=true`.\n\n\
         **Step B — Resolve from the BASE upward:**\n\
         ```\n\
         jj edit <lowest-conflicted-change-id>\n\
         ```\n\
         Fix the conflicted files in that commit (see structural-edit instructions below) so it \
         is conflict-free; descendants auto-rebase. Re-run the log from Step A and resolve the \
         next-lowest still-conflicted commit. Repeat until **no commit** in `::<branch>` has \
         `conflicts=true`.\n\n\
         **Step C — Verify before pushing:**\n\
         ```\n\
         jj log -r '::<branch>' -T 'conflict ++ \"\\n\"'\n\
         ```\n\
         Output must contain no `true`. Only then run `cube pr update --branch <branch>`.\n\n\
         **Do NOT** squash or resolve only at the working-copy tip — it cannot clear an \
         ancestor's conflict.\n\n\
         **Non-interactive env:** ALWAYS pass `-m \"…\"` to `jj describe`, `jj squash`, \
         `jj commit`, and `jj new`. The worker environment has no usable editor \
         (`EDITOR=false`); any jj command that opens an editor hard-fails with \
         \"Editor 'false' exited\". Never rely on the interactive editor.\n\n\
         **Structural edit — NOT line-range surgery:**\n\n\
         jj materializes each conflict as annotated regions directly in the file. \
         Resolve by **editing those regions in place**:\n\n\
         - Open the conflicted file and find the `<<<<<<<` / `>>>>>>>` marker blocks.\n\
         - Each block contains the conflict base and the two sides (`Contents of side #1`, \
         `Contents of side #2`). Decide which content to keep (or merge both), then replace \
         the entire marker block with the resolved content.\n\
         - Alternatively, run `jj resolve <file>` to open a 3-way merge tool (e.g. vimdiff) \
         that handles the structured regions for you.\n\n\
         **Anti-pattern — do NOT do this:** grep for conflict markers, extract specific line \
         ranges, and concatenate them to rebuild the file. That approach silently drops hunks \
         (off-by-one, missed sections) and makes the resolution look like a from-scratch \
         rewrite. Edit the marker regions directly instead.\n\n",
    );
    out.push_str("### Conflict diagnosis (from the engine's pre-spawn pass)\n\n");
    match attempt
        .conflict_diagnosis
        .as_deref()
        .map(serde_json::from_str::<ConflictDiagnosis>)
    {
        Some(Ok(diagnosis)) => out.push_str(&render_diagnosis_markdown(&diagnosis)),
        Some(Err(err)) => {
            out.push_str(&format!(
                "_Engine could not re-parse the diagnosis JSON (error: {err}). The\n\
                 raw blob is on `conflict_resolutions.conflict_diagnosis` if you need it._\n",
            ));
        }
        None => {
            out.push_str(
                "_No engine-collected diagnosis is available for this attempt. Use\n\
                 `jj st` and `jj resolve --list` after the rebase to discover the\n\
                 conflicts directly._\n",
            );
        }
    }
    out.push_str("\n### Stop conditions\n\n");
    out.push_str(
        "If any of the following applies, comment on the PR explaining the situation,\n\
         do NOT push, and run `boss engine conflicts mark-failed <attempt-id> --reason <r>`\n\
         with the appropriate reason — the engine will mark the attempt `failed`:\n\n\
            1. **Semantic obsolescence** — the upstream change accomplished what this PR\n   \
            was trying to do. Reason: `obsolescence_suspected`.\n\
            2. **Product decision required** — the conflict needs a human choice between\n   \
            two valid resolutions. Reason: `product_decision_required`.\n\
            3. **Architectural mismatch** — the upstream removed an abstraction this PR\n   \
            was extending. Reason: `architectural_mismatch`.\n\n\
         Do NOT close the PR yourself. Closing is the human's call.\n\n",
    );
    out.push_str(check_bypass_prohibition_text());
    out.push('\n');
    out.push_str("### Post-resolution PR comment\n\n");
    out.push_str(
        "After you push the resolution, post a PR comment. Build it from the template below, \
         but two sections are **computed from your actual resolution diff** — do not paste the \
         placeholders verbatim:\n\n",
    );
    out.push_str(
        "1. **⚠️ Removed section (required, removal-forward).** Compute the set of files and \
         exported surfaces this resolution DELETES relative to the pre-resolution PR head and \
         to `main`. Run `jj diff -r @ --summary` (and, if useful, `gh pr diff <n> --repo \
         <owner/repo>`) and list, prominently and near the top:\n   \
         - every file the resolution removes (status `D`), and\n   \
         - every exported symbol / public surface (function, component, type, route, flag) it \
         removes that a merged parent added.\n   \
         If the resolution removes NOTHING, write `Removed: none` explicitly — do not omit the \
         section. A removal that is not listed here is a defect. For each removal, add the \
         design-doc citation that authorizes it (per the preservation rule above); if you \
         cannot cite one, you should not be removing it — STOP and escalate instead of \
         commenting.\n\n",
    );
    out.push_str(
        "2. **Prior-approvals line (conditional — do NOT fabricate a review history).** Only \
         claim approvals were dismissed if a prior review actually existed. Check it \
         deterministically:\n   \
         ```\n   \
         gh api repos/<owner/repo>/pulls/<n>/reviews --jq 'length'\n   \
         ```\n   \
         - If the count is `> 0`: include the line \"Branch force-pushed; per branch \
         protection, prior approvals have been dismissed.\"\n   \
         - If the count is `0`: OMIT that line entirely (there were no approvals to dismiss — \
         stating otherwise fabricates a vetting history).\n\n",
    );
    out.push_str("Template:\n\n");
    out.push_str(
        "```\n\
         🤖 boss resolved merge conflicts on this PR after `main` moved.\n\n\
         Resolutions:\n\
         - <per-file resolution summary>\n\n\
         ⚠️ Removed (computed from the resolution diff):\n\
         - <removed file / exported surface + design-doc citation authorizing it, or `none`>\n\n\
         <conditional: only if `gh api .../pulls/<n>/reviews` length > 0>\n\
         Branch force-pushed; per branch protection, prior approvals have been dismissed.\n\
         Re-review when ready.\n\
         ```\n\n",
    );
    out
}

/// The two commands a conflict-resolution worker must run, in order,
/// before it is allowed to form any opinion about whether the conflict
/// still exists — plus the `jj` divergence hazard that makes local
/// reasoning unsound in a shared cube object store.
///
/// Exists because a worker gave up 26 seconds into its run, asserting
/// "this conflict was already resolved and pushed in a prior attempt",
/// having run neither of these commands (incident 2026-07-23,
/// spinyfin/mono#2070). GitHub reported `mergeable: CONFLICTING` /
/// `mergeStateStatus: DIRTY` the whole time. Divergent `jj` change ids
/// made two diagnostic revsets resolve to two *different* commits — one
/// conflict-free but stacked on stale `main`, one on current `main` but
/// conflicted — and the worker ANDed the two answers into a conclusion no
/// single commit supported. `jj` flagged the hazard with a `??` suffix
/// from its first line of output; nothing in the prompt this function
/// composes told the worker what that meant.
///
/// The engine enforces this independently at the Stop boundary
/// ([`crate::conflict_stop_gate`]) — this fragment is the cooperative
/// half, not the guarantee.
fn compose_conflict_ground_truth_fragment(attempt: &ConflictResolution) -> String {
    format!(
        "### Ground truth: run these FIRST, in this order (HARD GATE)\n\n\
         Before `jj log`, before `jj st`, before forming any opinion about whether this \
         conflict still exists:\n\n\
         **1. Ask GitHub — it is the only authority on whether this PR conflicts:**\n\n\
         ```\n\
         gh pr view {pr_url} --json mergeable,mergeStateStatus,headRefOid\n\
         ```\n\n\
         `mergeable: CONFLICTING` means the conflict is real and unresolved, no matter what \
         your local `jj` state suggests. `mergeable: UNKNOWN` means GitHub is still \
         recomputing — it is **not** a clean bill of health; re-run the query after the \
         rebase below. Only `mergeable: MERGEABLE` supports a claim that the conflict is \
         already resolved.\n\n\
         **2. Rebase — this is step 3 of your brief and it is not optional:**\n\n\
         ```\n\
         cube workspace rebase\n\
         ```\n\n\
         Its output (`REBASED_CLEAN` / `REBASED_WITH_CONFLICTS`) is the local ground truth. \
         Quote it in your final response.\n\n\
         **3. You may NOT conclude \"already resolved\" from local `jj` state alone.** \
         `conflicts=false` on some revset, \"the branch is a descendant of `main@origin`\", \
         and `jj git fetch` reporting \"Nothing changed\" are **not** evidence the conflict \
         is gone — a branch can satisfy all three while GitHub still reports `CONFLICTING`. \
         If you believe there is nothing to do, you must show the `gh pr view` output saying \
         `MERGEABLE` and the `cube workspace rebase` output saying `REBASED_CLEAN`. Without \
         both, keep working.\n\n\
         **4. Divergent change ids make local revsets lie.** Cube workspaces share one `jj` \
         object store, so the same change id can name several commits. If any `jj` output \
         shows a `??` suffix (e.g. `qtltpmoy??`), or `jj bookmark list` reports the \
         branch \"ahead by N commits, behind by M commits\" against `@git`, that change is \
         **DIVERGENT**: change-id revsets resolve to an arbitrary copy, so every \
         `conflicts=` and `descendants()` answer you get is unsound, and two revsets in the \
         same session can silently answer about two different commits. Re-run every check \
         using **full commit ids**, never change ids; `jj edit <change-id>` will also \
         hard-fail with \"resolved to more than one revision\". Do not `jj abandon` a commit \
         you did not create in this run — every `mono-agent-*` workspace shares one `jj` \
         object store, so a duplicate you did not make may be another worker's live \
         in-progress commit.\n\n\
         **5. A non-zero exit from any `jj` or `gh` command in this section invalidates \
         whatever conclusion you were gathering.** Do not fall back to reasoning from stale \
         output, a partial result, or local state alone — re-run the command (after fixing \
         the underlying cause, if the failure is not transient) and only draw a conclusion \
         from a command that actually exited 0.\n\n",
        pr_url = attempt.pr_url,
    )
}

/// Signal-specific fragment appended to `compose_revision_directive` when the
/// revision was created with `created_via = "ci-fix:<crm_id>"`.
///
/// Provides the CI remediation context (failing checks, log excerpt, playbook)
/// that the worker needs to fix the failing CI — identical in content to the
/// bespoke `compose_ci_remediation_prompt` except that the branch/push spine
/// is already covered by the shared revision directive.
fn compose_ci_remediation_fragment(attempt: &CiRemediation) -> String {
    let is_rebounce = attempt.failure_kind.as_deref() == Some("merge_queue_rebounce");
    let is_trunk_eviction = attempt.failure_kind.as_deref() == Some("trunk_queue_eviction");
    // Both share the "PR's own head-branch CI is green" property — see
    // `ci_watch::is_queue_side_failure_kind`.
    let is_queue_side_failure = is_rebounce || is_trunk_eviction;

    let mut out = String::new();
    out.push_str("\n---\n\n");

    if is_rebounce {
        out.push_str(&format!(
            "## CI remediation context: PR #{pr_num} ({kind}) — merge-queue FAILED_CHECKS\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
        out.push_str(
            "> **Important**: this is a **merge-queue rebounce**, not a per-PR CI failure.\n\
             > - The PR's own required checks are **green** on its head SHA.\n\
             > - **`gh pr checks` will show green** — this is expected and does NOT mean CI passed.\n\
             >   Do NOT run `gh pr checks` and conclude there is nothing to fix. The actual failing\n\
             >   build is on the **synthetic merge commit** on a `gh-readonly-queue/*` branch,\n\
             >   listed under \"Failing required checks\" below with its build URL and job id.\n\
             > - Root cause: something landed on `main` between this PR's CI run and its queue turn\n\
             >   that is semantically incompatible. After fixing, **re-enqueue** the PR.\n\n",
        );
    } else if is_trunk_eviction {
        out.push_str(&format!(
            "## CI remediation context: PR #{pr_num} ({kind}) — Trunk merge-queue eviction\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
        out.push_str(
            "> **Important**: this PR was **evicted from the Trunk merge queue**, not a per-PR CI failure.\n\
             > - The PR's own required checks are **green** on its head SHA.\n\
             > - **`gh pr checks` will show green** — this is expected and does NOT mean CI passed.\n\
             >   Do NOT run `gh pr checks` and conclude there is nothing to fix. The actual failing\n\
             >   build ran on Trunk's ephemeral `trunk-merge/pr-<N>/<uuid>` construction branch,\n\
             >   listed under \"Failing required checks\" below with its build URL and job id.\n\
             > - Root cause: something landed on the target branch between this PR's CI run and its\n\
             >   queue turn that is semantically incompatible. After fixing and pushing, just push and\n\
             >   stop — Boss auto-resubmits the PR to the Trunk queue on the next poller pass once this\n\
             >   revision reaches `done`. Do NOT ask a human to re-run the merge, do NOT comment\n\
             >   `/trunk merge` yourself, and do NOT run `gh pr merge` — that bypasses the queue and\n\
             >   races the automatic resubmit.\n\n",
        );
    } else {
        out.push_str(&format!(
            "## CI remediation context: PR #{pr_num} ({kind}) — required checks failing\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
    }

    if !attempt.head_branch.is_empty() {
        out.push_str(&format!("**Branch**: `{}`\n", attempt.head_branch));
    }
    if is_rebounce && let Some(ref sha) = attempt.before_commit_sha {
        out.push_str(&format!("**Synthetic merge SHA** (fetch CI logs from here): `{sha}`\n",));
    }
    out.push_str(&format!("**Head sha at trigger**: `{}`\n", attempt.head_sha_at_trigger,));
    out.push_str(&format!("**Attempt id**: `{}`\n\n", attempt.id));

    out.push_str("### Failing required checks\n\n");
    match render_failed_checks_markdown(&attempt.failed_checks) {
        Some(md) => out.push_str(&md),
        None => {
            if is_rebounce {
                let sha_hint = attempt.before_commit_sha.as_deref().unwrap_or("<synthetic-merge-sha>");
                out.push_str(&format!(
                    "_The engine did not capture the failing checks for this merge-queue rebounce. \
                     Do NOT use `gh pr checks` — it shows the PR-head checks, which are green. \
                     Instead, fetch the check runs for the synthetic merge SHA directly: \
                     `gh api repos/<owner>/<repo>/commits/{sha_hint}/check-runs \
                     | jq '.check_runs[] | select(.conclusion == \"failure\") | {{name, details_url}}'`._\n",
                ));
            } else if is_trunk_eviction {
                out.push_str(&format!(
                    "_The engine did not capture the failing checks for this Trunk queue eviction. \
                     Do NOT use `gh pr checks` — it shows the PR-head checks, which are green. \
                     Instead, discover the failing build directly on Buildkite (org-wide, since more \
                     than one pipeline may build the episode branch): \
                     `bk api \"/builds?branch=trunk-merge/pr-{pr_num}/<episode-uuid>\"` if the episode \
                     uuid is known, otherwise `bk api \"/builds?state[]=failed&state[]=failing&per_page=100\"` \
                     (paginate with `&page=N` if needed) filtered client-side to a branch starting with \
                     `trunk-merge/pr-{pr_num}/` \
                     (do NOT match `trunk-temp/*` — that is a different, non-gating branch)._\n",
                    pr_num = attempt.pr_number,
                ));
            } else {
                out.push_str(
                    "_The engine did not record a parseable `failed_checks` blob for this attempt. \
                     Read `gh pr checks` to enumerate the failing required checks before deciding the fix._\n",
                );
            }
        }
    }
    out.push('\n');

    if let Some(bk_cmds) = render_bk_log_commands(&attempt.failed_checks) {
        out.push_str(&bk_cmds);
    }

    if !is_queue_side_failure {
        out.push_str("### If CI is already green (nothing to fix)\n\n");
        out.push_str(&format!(
            "Before assuming there is work to do, check the **current** state of the PR's required \
             checks (`gh pr checks {pr}` / `gh pr view {pr}`). If they are **already passing** — the \
             failure cleared on its own (a flaky check settled, `main` moved, or a stale failure was \
             re-detected) — you do NOT have to invent a fix. Declare it; the engine VALIDATES your \
             claim against live CI before retiring the attempt:\n\n\
             ```\n\
             boss engine ci mark-noop --attempt-id {attempt} --observed-sha <current-head-sha> --reason already-green\n\
             ```\n\n\
             The engine independently re-probes live CI for the PR's current head SHA. If every \
             required check is verified green, the attempt is retired and the parent unblocks — you are \
             **done, stop**. If CI is still red or pending, the command **fails** (non-zero exit) with \
             the live status and the attempt stays open: the failure is real, so continue below.\n\n",
            pr = attempt.pr_url,
            attempt = attempt.id,
        ));
    }

    if attempt.attempt_kind == "retrigger" {
        out.push_str("### Action: retrigger the failing build\n\n");
        out.push_str(
            "The engine has pre-classified this failure as infra (every failing check has \
             `conclusion ∈ {STARTUP_FAILURE, CANCELLED}`). No log read or code change is needed.\n\n",
        );
        out.push_str(
            "1. Re-run the failing build via the per-provider CLI (`bk build retry <build-id>` \
             for Buildkite or `gh run rerun <run-id> --failed` for GitHub Actions). The failing \
             check's `target_url` above carries the right id.\n\
             2. Call `boss engine ci mark-retriggered --attempt-id <attempt-id> --new-id <new-build-or-run-id>` \
             so the engine records the new run id and stays out of the budget path. Do NOT call \
             `mark-failed` or push code.\n\
             3. Stop. The merge-poller will observe the re-run's outcome on the next sweep.\n\n",
        );
    } else {
        if is_rebounce {
            out.push_str("### Action: rebase onto current main, then fix the semantic conflict\n\n");
            out.push_str(
                "A merge-queue rebounce almost always means something landed on `main` between \
                 this PR's CI run and its queue turn that is **semantically incompatible**.\n\
                 Fix is: rebase, look at the CI failure on the synthetic merge SHA, add a focused \
                 fix, push, and re-enqueue the PR.\n\n",
            );
        } else if is_trunk_eviction {
            out.push_str("### Action: rebase onto the target branch, then fix the semantic conflict\n\n");
            out.push_str(
                "A Trunk queue eviction almost always means something landed on the target branch \
                 between this PR's CI run and its queue turn that is **semantically incompatible**.\n\
                 Fix is: rebase, look at the CI failure on Trunk's construction branch, add a focused \
                 fix, push, and get the PR resubmitted to the queue.\n\n",
            );
        } else {
            out.push_str("### Action: rebase first, then fix\n\n");
            out.push_str(
                "Many CI failures on long-running PRs are caused by `main` moving. The cheapest \
                 experiment is rebasing onto `main` HEAD before changing any code — if CI goes \
                 green after the rebase, no fix-attempt slot is consumed.\n\n",
            );
        }
        out.push_str("**Step 1 — Rebase onto base HEAD and force-push** (replaces step 3 above).\n\n");
        out.push_str(&format!(
            "```\n\
             jj edit {branch}\n\
             jj rebase -d main -b {branch}\n\
             # then push via step 5 of the revision directive\n\
             ```\n\n",
            branch = if attempt.head_branch.is_empty() {
                "<branch>"
            } else {
                attempt.head_branch.as_str()
            },
        ));
        out.push_str(
            "**If the rebase produces conflicts on a stacked branch:** jj records conflicts \
             IN each commit independently — `jj git push` refuses to push ANY commit that \
             still contains a conflict, including ancestors. Resolving only the tip does NOT \
             clear ancestor conflicts. List conflicted commits and resolve from the base upward:\n\
             ```\n\
             # list conflicted commits\n\
             jj log -r '::<branch>' -T 'change_id ++ \" \" ++ description.first_line() ++ \" conflicts=\" ++ conflict ++ \"\\n\"'\n\
             # edit the lowest conflicted commit, fix it, repeat upward\n\
             jj edit <lowest-conflicted-change-id>\n\
             # verify: output must contain no 'true'\n\
             jj log -r '::<branch>' -T 'conflict ++ \"\\n\"'\n\
             ```\n\
             Always pass `-m \"…\"` to `jj describe`/`jj squash`/`jj commit`/`jj new` — \
             `EDITOR=false` in this environment; any command that opens an editor hard-fails.\n\n",
        );
        if is_rebounce {
            out.push_str(
                "Wait for the re-run's required checks to settle (`gh pr checks --watch`). Then:\n\n\
                 - **If post-rebase CI is green**, do NOT call `mark-succeeded-via-rebase` — rebounce \
                 attempts are not validatable via head-branch CI (the engine's guard rejects that verb \
                 unconditionally for this attempt class). Instead re-enqueue the PR directly \
                 (`gh pr merge --auto --squash`) and stop; the merge-poller retires the attempt when \
                 the queue outcome is observed.\n\
                 - **If post-rebase CI is still red**, the semantic conflict requires a code fix — \
                 continue to Step 2.\n\n",
            );
        } else if is_trunk_eviction {
            out.push_str(
                "Wait for the re-run's required checks to settle (`gh pr checks --watch`). Then:\n\n\
                 - **If post-rebase CI is green**, do NOT call `mark-succeeded-via-rebase` — Trunk \
                 eviction attempts are not validatable via head-branch CI (the engine's guard rejects \
                 that verb unconditionally for this attempt class). Push the fix and stop — do NOT ask \
                 a human and do NOT run `gh pr merge`; Boss auto-resubmits the PR to the Trunk queue on \
                 the next poller pass once this revision reaches `done`, and the poller retires the \
                 attempt when the queue outcome is observed.\n\
                 - **If post-rebase CI is still red**, the semantic conflict requires a code fix — \
                 continue to Step 2.\n\n",
            );
        } else {
            out.push_str(
                "Wait for the re-run's required checks to settle (`gh pr checks --watch`). Then:\n\n\
                 - **If post-rebase CI is green**, call \
                 `boss engine ci mark-succeeded-via-rebase --attempt-id <attempt-id>`. The engine \
                 independently re-probes live CI for the PR's current head SHA before honoring this — \
                 calling it early or on a red head gets a rejection (non-zero exit), not a recorded \
                 success, so actually wait for `--watch` to finish. On a verified-green response, stop; \
                 the engine flips the attempt to `succeeded`, sets `consumes_budget = 0`, and decrements \
                 `tasks.ci_attempts_used` so this attempt does not count against the PR's budget.\n\
                 - **If post-rebase CI is still red**, continue to Step 2. The budget slot is now \
                 consumed; this is the fix attempt the engine pre-classified.\n\n",
            );
        }

        out.push_str("**Step 2 — Read the log, classify, fix, push.**\n\n");
        if is_rebounce {
            let sha_hint = attempt.before_commit_sha.as_deref().unwrap_or("<synthetic-merge-sha>");
            out.push_str(&format!(
                "The failing job ran on the **synthetic merge SHA `{sha_hint}`** \
                 (`gh-readonly-queue/*` branch), NOT the PR head. \
                 Use the pre-filled commands in \"Ready-to-run Buildkite log commands\" above \
                 if shown; otherwise fall back to the provider CLI:\n\n\
                 - Buildkite: `bk job log --pipeline <slug> --build-number <N> <job-uuid>` \
                 (slug and build number are in the check's `target_url` above; job UUIDs come \
                 from `bk build view <N> --pipeline <slug>`)\n\
                 - GitHub Actions: `gh run view --log-failed --job <job-id>` \
                 (job id from the failing check URL above)\n\n",
            ));
            out.push_str("Engine-collected log excerpt (from the synthetic merge commit's failing job):\n\n");
            match attempt.log_excerpt.as_deref().map(str::trim) {
                Some(tail) if !tail.is_empty() => {
                    out.push_str("```\n");
                    out.push_str(tail);
                    out.push_str("\n```\n\n");
                }
                _ => {
                    out.push_str(&format!(
                        "_No pre-fetched log excerpt is available for this attempt. \
                         Use the commands above to fetch directly from the synthetic merge \
                         SHA `{sha_hint}`._\n\n",
                    ));
                }
            }
        } else if is_trunk_eviction {
            out.push_str(&format!(
                "The failing job ran on Trunk's **ephemeral construction branch** \
                 `trunk-merge/pr-{pr_num}/<uuid>`, NOT the PR head. \
                 Use the pre-filled commands in \"Ready-to-run Buildkite log commands\" above \
                 if shown; otherwise discover the build via \
                 `bk api \"/builds?state[]=failed&state[]=failing&per_page=100\"` (paginate with `&page=N` \
                 if needed) filtered to a branch starting with `trunk-merge/pr-{pr_num}/`, then \
                 `bk job log --pipeline <slug> --build-number <N> <job-uuid>`.\n\n",
                pr_num = attempt.pr_number,
            ));
            out.push_str("Engine-collected log excerpt (from the Trunk construction branch's failing job):\n\n");
            match attempt.log_excerpt.as_deref().map(str::trim) {
                Some(tail) if !tail.is_empty() => {
                    out.push_str("```\n");
                    out.push_str(tail);
                    out.push_str("\n```\n\n");
                }
                _ => {
                    out.push_str(
                        "_No pre-fetched log excerpt is available for this attempt. \
                         Use the commands above to fetch directly from Trunk's construction branch \
                         build._\n\n",
                    );
                }
            }
        } else {
            out.push_str("Engine-collected log excerpt (failing job tail):\n\n");
            match attempt.log_excerpt.as_deref().map(str::trim) {
                Some(tail) if !tail.is_empty() => {
                    out.push_str("```\n");
                    out.push_str(tail);
                    out.push_str("\n```\n\n");
                }
                _ => {
                    out.push_str(
                        "_The engine's pre-spawn log fetch did not produce an excerpt for this attempt. \
                         Use the ready-to-run commands above (`bk job log --pipeline …`) or \
                         `gh run view --log-failed --job <job-id>` (job id from the failing check URL)._\n\n",
                    );
                }
            }
        }
        out.push_str(
            "1. Classify the failure with `boss engine ci classify --attempt-id <attempt-id> --class <tractable|flaky_or_infra|unfixable>`.\n   \
                - `tractable` → there's a clear code change that resolves it. Make it. Push.\n   \
                - `flaky_or_infra` → the failure is environmental. Pivot to the retrigger playbook \
                (re-run the failing build via the provider CLI and call `mark-retriggered`).\n   \
                - `unfixable` → the failure is real and out of scope. Call \
                `boss engine ci mark-failed --attempt-id <attempt-id> --reason <reason>` \
                and stop. Do NOT push.\n",
        );
        out.push_str("2. No `test_command` context is available here; rely on CI to verify the push.\n");
        out.push_str(&format!(
            "3. Push your fix via step 5 of the revision directive (push to the parent branch \
                `{branch}`). The merge-poller will observe the new head sha and re-evaluate CI on \
                the next sweep — when green it flips the attempt to `succeeded` and unblocks the parent.\n\n",
            branch = if attempt.head_branch.is_empty() {
                "<branch>"
            } else {
                attempt.head_branch.as_str()
            },
        ));
        if is_rebounce {
            out.push_str(
                "**Step 3 (after CI is green) — Re-enqueue the PR.**\n\n\
                 The merge queue does **not** auto-retry after a dequeue. After your push produces \
                 green CI, re-add the PR to the merge queue:\n\n\
                 ```\n\
                 gh pr merge --auto --squash  # or --merge / --rebase per repo policy\n\
                 ```\n\n",
            );
        } else if is_trunk_eviction {
            out.push_str(
                "**Step 3 — nothing further to do here.**\n\n\
                 The Trunk queue does **not** auto-retry after an eviction, but Boss's own poller does: \
                 once this revision reaches `done`, it auto-resubmits the PR to the Trunk queue on its \
                 next pass. Push your fix and stop — do NOT ask a human to resubmit, do NOT comment \
                 `/trunk merge` yourself, and do NOT run `gh pr merge`; either would race the automatic \
                 resubmit.\n\n",
            );
        }
    }

    out.push_str("### Stop conditions\n\n");
    out.push_str(
        "- **You are not adding scope.** The only allowed change is one that makes the failing \
         required checks pass (rebase, infra retrigger, or a focused fix).\n\
         - **Do not close the PR yourself.** Closing is the human's call.\n\
         - **Always pass `-m \"…\"` to `jj describe` / `jj squash`.** The worker \
         environment has no usable `$EDITOR`.\n\n",
    );
    out.push_str(check_bypass_prohibition_text());
    out.push('\n');
    out
}

/// Templated prompt for the `ci_remediation` execution kind, retrigger path
/// only. `fix`-kind CI attempts now dispatch through the revision substrate
/// (`revision_implementation`); only `retrigger` (design Q6: no commit,
/// not revision-shaped) still uses this bespoke execution kind.
fn compose_ci_remediation_prompt(
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    cube_change_id: Option<&str>,
    attempt: &CiRemediation,
    _test_command: Option<&str>,
) -> String {
    let mut prompt = String::new();

    prompt.push_str(&format!(
        "## CI remediation: PR #{pr_num} ({kind}) — required checks failing\n\n",
        pr_num = attempt.pr_number,
        kind = attempt.attempt_kind,
    ));

    prompt.push_str(&format!("**PR**: {}\n", attempt.pr_url));
    if !attempt.head_branch.is_empty() {
        prompt.push_str(&format!("**Branch**: `{}`\n", attempt.head_branch));
    }
    prompt.push_str(&format!("**Head sha at trigger**: `{}`\n", attempt.head_sha_at_trigger,));
    prompt.push_str(&format!("**Workspace**: `{}`\n", workspace_path.display()));
    prompt.push_str(&format!("**Attempt id**: `{}`\n", attempt.id));
    prompt.push_str(&format!("**Execution id**: `{}`\n", execution.id));
    if let Some(change) = cube_change_id {
        prompt.push_str(&format!("**Local change**: `{change}`\n"));
    }
    prompt.push_str(&format!("**Work item**: `{}`\n\n", work_item_name(work_item),));

    // Failing-check list — same JSON the engine seeded on the row at
    // detection time. Rendered as a bulleted summary; the worker has the
    // raw `failed_checks` field if it wants to read further.
    prompt.push_str("### Failing required checks\n\n");
    match render_failed_checks_markdown(&attempt.failed_checks) {
        Some(md) => prompt.push_str(&md),
        None => prompt.push_str(
            "_The engine did not record a parseable `failed_checks` blob for this attempt. \
             Read `gh pr checks` to enumerate the failing required checks before deciding the fix._\n",
        ),
    }
    prompt.push('\n');

    // If the failure already cleared, the worker can declare a
    // validated noop rather than retriggering a build that is no longer
    // red. The engine re-probes live CI before honoring it.
    //
    // Gated `!is_rebounce` to match the sibling revision-fragment brief
    // (`compose_ci_remediation_fragment`): a merge_queue_rebounce
    // failure lives on the synthetic merge commit, so the PR's
    // head-branch checks always read green — surfacing `mark-noop` to a
    // rebounce worker would invite a claim the engine is guaranteed to
    // reject (`handle_mark_ci_remediation_noop` refuses rebounce
    // attempts before it even probes). Rebounce rows normally deliver
    // via a revision rather than this bespoke prompt, but the
    // stranded-rescue path can re-dispatch one here, so guard it.
    let is_rebounce = attempt.failure_kind.as_deref() == Some("merge_queue_rebounce");
    if !is_rebounce {
        prompt.push_str("### If CI is already green (nothing to fix)\n\n");
        prompt.push_str(&format!(
            "Check the **current** required checks first (`gh pr checks {pr}`). If they are already \
             passing, declare it instead of retriggering — the engine validates the claim against live \
             CI before retiring the attempt:\n\n\
             ```\n\
             boss engine ci mark-noop --attempt-id {attempt} --observed-sha <current-head-sha> --reason already-green\n\
             ```\n\n\
             Verified green → attempt retired, parent unblocked, you are done. Still red/pending → the \
             command fails (non-zero) and the attempt stays open; fall through to the retrigger playbook.\n\n",
            pr = attempt.pr_url,
            attempt = attempt.id,
        ));
    }

    // §Q4 retrigger playbook: every failure is unambiguous infra,
    // no log read needed, no code change.
    prompt.push_str("### Action: retrigger the failing build\n\n");
    prompt.push_str(
        "The engine has pre-classified this failure as infra (every failing check has \
         `conclusion ∈ {STARTUP_FAILURE, CANCELLED}`). No log read or code change is needed.\n\n",
    );
    prompt.push_str(
        "1. Re-run the failing build via the per-provider CLI (`bk build retry <build-id>` \
         for Buildkite or `gh run rerun <run-id> --failed` for GitHub Actions). The failing \
         check's `target_url` above carries the right id.\n\
         2. Call `boss engine ci mark-retriggered --attempt-id <attempt-id> --new-id <new-build-or-run-id>` \
         so the engine records the new run id and stays out of the budget path. Do NOT call \
         `mark-failed` or push code.\n\
         3. Stop. The merge-poller will observe the re-run's outcome on the next sweep.\n\n",
    );

    prompt.push_str("### Stop conditions\n\n");
    prompt.push_str(
        "- **You are not adding scope.** The only allowed change is one that makes the failing \
         required checks pass (infra retrigger only — no code changes).\n\
         - **Do not close the PR yourself.** Closing is the human's call.\n\n",
    );
    prompt.push_str("Respond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
}

/// Build a block of ready-to-run `bk` CLI commands for every Buildkite
/// entry in the `failed_checks` JSON. Returns `None` when the JSON
/// contains no Buildkite entries or the target URLs lack enough
/// information to construct pre-filled commands.
///
/// Emits two commands per failing Buildkite job:
///   `bk build view <N> --pipeline <slug>`  — enumerate all jobs in the build
///   `bk job log --pipeline <slug> --build-number <N> <job-uuid>`
fn render_bk_log_commands(failed_checks_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        target_url: String,
        provider: String,
        #[serde(default)]
        provider_job_id: Option<String>,
    }
    let entries: Vec<Entry> = serde_json::from_str(failed_checks_json).ok()?;

    let mut commands = String::new();
    for e in &entries {
        if e.provider != "buildkite" {
            continue;
        }
        let Some(pipeline) = parse_buildkite_pipeline_slug(&e.target_url) else {
            continue;
        };
        let Some(build_num) = parse_buildkite_build_id(&e.target_url) else {
            continue;
        };
        commands.push_str(&format!("bk build view {build_num} --pipeline {pipeline}\n",));
        match e.provider_job_id.as_deref() {
            Some(job_id) => {
                commands.push_str(&format!(
                    "bk job log --pipeline {pipeline} --build-number {build_num} {job_id}\n",
                ));
            }
            None => {
                commands.push_str(&format!(
                    "# (replace <job-uuid> with an id from `bk build view` above)\n\
                     bk job log --pipeline {pipeline} --build-number {build_num} <job-uuid>\n",
                ));
            }
        }
    }

    if commands.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("### Ready-to-run Buildkite log commands\n\n");
    out.push_str(
        "`bk` is the Buildkite CLI. These commands are pre-filled with the \
         pipeline, build number, and job id — no argument guessing required:\n\n",
    );
    out.push_str("```\n");
    out.push_str(&commands);
    out.push_str("```\n\n");
    Some(out)
}

/// Render the `failed_checks` JSON blob (one entry per failing required
/// check at trigger time) as a small bulleted list for the worker
/// prompt. Returns `None` when the blob is missing or malformed — the
/// caller falls back to a generic instruction.
fn render_failed_checks_markdown(failed_checks_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        conclusion: String,
        target_url: String,
        provider: String,
        #[serde(default)]
        provider_job_id: Option<String>,
    }
    let entries: Vec<Entry> = serde_json::from_str(failed_checks_json).ok()?;
    if entries.is_empty() {
        return None;
    }
    let mut out = String::new();
    for e in &entries {
        out.push_str(&format!(
            "- `{name}` — {conclusion} ({provider}): {url}",
            name = e.name,
            conclusion = e.conclusion,
            provider = e.provider,
            url = e.target_url,
        ));
        if let Some(job_id) = e.provider_job_id.as_deref() {
            out.push_str(&format!(" (job `{job_id}`)"));
        }
        out.push('\n');
    }
    Some(out)
}

fn render_diagnosis_markdown(diagnosis: &ConflictDiagnosis) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Schema v{}. Base sha `{}`, dependent head sha `{}`.\n\n",
        diagnosis.schema_version, diagnosis.base_sha, diagnosis.head_sha,
    ));
    if let Some(err) = diagnosis.error.as_deref() {
        out.push_str(&format!(
            "_Engine-side probe failed: {err}. The list below may be incomplete; trust\n\
             `jj st` after the rebase as the source of truth._\n\n",
        ));
    }
    if diagnosis.files.is_empty() {
        if diagnosis.error.is_none() {
            out.push_str(
                "_No conflicted files reported by the engine's pre-spawn probe. The\n\
                 conflict may have been transient; run `jj rebase` and trust `jj st`._\n",
            );
        }
        return out;
    }
    out.push_str(&format!("Conflicted files ({}):\n\n", diagnosis.files.len()));
    for file in &diagnosis.files {
        out.push_str(&format!("- `{}` — {}", file.path, file.shape));
        if let Some(count) = file.marker_count {
            out.push_str(&format!(" ({count} marker block(s))"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod compose_prompt_tests;
