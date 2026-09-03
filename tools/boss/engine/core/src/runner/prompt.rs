//! Worker prompt composition: [`compose_execution_prompt`] and the directive /
//! fragment builder family (Bazel gates, editorial rules, escalation /
//! deferred-scope / no-op / CI-monitoring directives, and the design /
//! investigation / revision / conflict / CI-remediation fragments).

use std::path::Path;

use crate::ci_log_reader::{parse_buildkite_build_id, parse_buildkite_pipeline_slug};
use crate::conflict_diagnosis::ConflictDiagnosis;
use crate::structured_output::StructuredOutputKind;
use crate::work::{
    CiRemediation, ConflictResolution, Project, WorkDb, WorkExecution, WorkItem, parse_pr_doc_artifact_id,
};
use boss_protocol::{EditorialRules, ExecutionKind, TaskKind, TemplatePolicy};

use super::work_item::{project_details, work_item_details, work_item_name, work_item_pr_url};

mod design;
use design::{compose_design_directive, compose_design_postmortem_directive};

#[derive(bon::Builder)]
pub(super) struct ExecutionPromptParams<'a> {
    execution: &'a WorkExecution,
    work_item: &'a WorkItem,
    workspace_path: &'a Path,
    parent_project: Option<&'a Project>,
    cube_change_id: Option<&'a str>,
    conflict_attempt: Option<&'a ConflictResolution>,
    ci_attempt: Option<&'a CiRemediation>,
    editorial_rules: Option<&'a EditorialRules>,
    /// `products.design_guidance` — markdown injected into the
    /// `[product-design-guidance]` block of the design-family directive
    /// only (`compose_design_directive` / `compose_design_postmortem_directive`).
    /// `None` / empty → no block, today's behaviour. Distinct from
    /// `editorial_rules` (GitHub-visible-surface rules, every kind) and
    /// `dispatch_preamble` (every kind, rendered outside this builder
    /// entirely — see `worker_spawn.rs`).
    design_guidance: Option<&'a str>,
    pr_template_set: &'a crate::pr_template::PrTemplateSet,
    #[builder(default)]
    editorial_enabled: bool,
    /// Whether `worker_signal_proposals_seam` is on — gates the worker-facing
    /// prompt half of this seam
    /// (`worker-proposal-api-replace-fragile-worker-to-engine-seams.md`):
    /// [`worker_escalation_protocol_directive`], the two Bazel pre-push gate
    /// blurbs, and [`no_op_completion_directive`]'s pointer. `false` (the
    /// flag's registry default) reproduces the exact marker-only
    /// text; `true` teaches the `"$BOSS_BIN" propose` verbs instead — see those
    /// functions' docs. This is the OTHER half of the flag: the engine's
    /// read path (`crate::completion::WorkerCompletionHandler::detect_and_file_worker_signals`)
    /// is gated by the same flag name read directly from
    /// `FeatureFlagsStore`; gating the prompt too is what makes "flag off"
    /// restore today's behavior exactly, prompt included — a worker must
    /// never be taught a verb the engine won't yet read proposals-first for.
    #[builder(default)]
    worker_signal_proposals_seam_enabled: bool,
    /// Whether `deferred_scope_proposals_seam` is on — gates the worker-facing
    /// prompt half of the deferred-scope seam migration (design
    /// implementation task 9): [`deferred_scope_directive`]. `false` (the
    /// flag's registry default) reproduces the exact marker-only text;
    /// `true` teaches `"$BOSS_BIN" propose deferred-scope` instead. This is the
    /// OTHER half of the flag: the engine's read path
    /// (`crate::completion::WorkerCompletionHandler::detect_and_record_deferred_scope`)
    /// is gated by the same flag name read directly from
    /// `FeatureFlagsStore`; gating the prompt too is what makes "flag off"
    /// restore today's behavior exactly, prompt included.
    #[builder(default)]
    deferred_scope_proposals_seam_enabled: bool,
    /// Whether `followup_proposals_seam` is on — gates the worker-facing
    /// prompt half of the follow-ups seam migration (design implementation
    /// task 10): [`followups_emission_block`]. `false` (the flag's registry
    /// default) reproduces the exact structured-output-artifact-primary /
    /// `FOLLOWUPS:`-sentinel-fallback text; `true` teaches `"$BOSS_BIN" propose
    /// followup-task` instead. This is the OTHER half of the flag:
    /// `crate::completion::pr_transition`'s followups block, which counts a
    /// fallback hit whenever its legacy chain lands a follow-up not already
    /// covered by a `followup_task` proposal, is gated by the same flag
    /// name read directly from `FeatureFlagsStore`; gating the prompt too
    /// is what makes "flag off" restore today's behavior exactly, prompt
    /// included.
    #[builder(default)]
    followup_proposals_seam_enabled: bool,
    /// Whether `run_done_proposals_seam` is on — gates the worker-facing
    /// prompt half of the terminal-declaration migration:
    /// [`run_done_directive`], and the sentence
    /// [`pr_terminal_directive`] adds about declaring before the terminal
    /// push. `false` (the flag's registry default) renders today's prompt
    /// verbatim, with no mention of `boss propose done`; `true` teaches the
    /// verb. This is the OTHER half of the flag: the engine's
    /// `evaluate_satisfied_deliverable_on_stop` gate and the
    /// `NO_CHANGES_NEEDED` proposals-first read are gated by the same flag
    /// name read directly from `FeatureFlagsStore`. Teaching the verb while
    /// the engine still infers would be harmless but pointless; gating the
    /// engine while the worker is never told the verb would hold every run
    /// to the backstop, so the two must move together.
    #[builder(default)]
    run_done_proposals_seam_enabled: bool,
    /// Already-merged `merge_order` siblings whose surfaces this forward-port
    /// must preserve (rendered lines). Empty for non-conflict revisions and
    /// for conflict revisions with no merged overlap partner.
    #[builder(default)]
    merge_order_preservation: &'a [String],
}

/// Render the `## STARTUP RECOVERY` block for a worker respawned after its
/// predecessor was interrupted.
///
/// ## Why this only fires on a durable pointer
///
/// The engine's operating rule for recovery is that it fires only on an
/// unambiguous durable pointer the system itself wrote — restart fresh on
/// doubt. The old block violated that: alongside genuinely recovered state it
/// also told the worker "the prior worker **may** have pushed commits to
/// `boss/exec_<prior-id>`" and handed it a `jj edit <branch>@origin` line to
/// try. That branch name is *derived*, not *recorded* — the engine has no
/// column anywhere that confirms a push actually happened for an orphaned
/// execution (`pr_url` is only ever stamped atomically with the transition to
/// `completed`, which an orphaned execution never reaches). So the line was a
/// name-match heuristic dressed up as a resume instruction, and it fails
/// loudly and pointlessly whenever the prior worker died before pushing —
/// which is the common case, not the exception.
///
/// The only thing the engine *does* durably record is [recovered workspace
/// state](boss_engine_recovery::recovery_apply): a marker
/// (`.boss/recovery-report.json`) it writes itself when it actually recovers
/// something, in place or from a saved patch. This function is now called
/// only when that marker exists for this execution — see
/// [`compose_execution_prompt`]. When it doesn't, [`compose_execution_prompt`]
/// renders no block at all: the ordinary "expected branch name" / `jj new
/// main` guidance already in the prompt is the correct, honest instruction
/// for a fresh start, and no extra text is needed to say so.
///
/// ## What the block says
///
/// 1. whether state was recovered, and how — in place by cube (jj history
///    intact) or replayed from a patch (uncommitted edits only);
/// 2. what exactly was restored, in files and line counts, so the worker can
///    check rather than guess;
/// 3. to **inspect before building on it** — recovered work is a crashed
///    worker's mid-thought, not a reviewed baseline, and must not be reset.
///
/// A `patch_error` on the report means recovery FAILED. That case gets its
/// own paragraph telling the worker not to assume anything was resumed —
/// silence there would leave it guessing, which is how a "recovered" worker
/// quietly redoes everything or, worse, half-redoes it.
fn startup_recovery_block(report: &boss_engine_recovery::recovery_apply::RecoveryReport) -> String {
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

/// Explain the exact workspace handoff for a review revision converted to a
/// followup because its parent PR merged mid-run. This keys primarily on the
/// dedicated execution shape written by `reconcile_work_item_execution` — a
/// chore-implementation followup with a soft dirty-workspace preference,
/// which no other mint produces — not on task kind alone:
/// `resolve_revision_on_parent_close` (work/chain_helpers.rs) falls back
/// from `TaskKind::Followup` to plain `TaskKind::Chore` when the chain
/// root's PR URL is missing or unparseable, and that chore-fallback
/// conversion still inherits the same workspace/allow_dirty shape, so it
/// needs this brief too.
fn merge_cancelled_review_recovery_block(
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
) -> Option<String> {
    let task = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) if matches!(task.kind, TaskKind::Followup | TaskKind::Chore) => {
            task
        }
        _ => return None,
    };
    if execution.kind != ExecutionKind::ChoreImplementation || !execution.allow_dirty || !execution.prefer_is_soft {
        return None;
    }
    let preferred = execution.preferred_workspace_id.as_deref()?;
    let origin = task
        .origin_pr_number
        .map(|number| format!("PR #{number}"))
        .unwrap_or_else(|| "the merged origin PR".to_owned());
    let current = execution.cube_workspace_id.as_deref();

    // `reconcile_workspace_recovery` (coordinator/execution.rs) already
    // resolved whether the re-leased workspace's dirty state was actually
    // confirmed — it writes this marker with `RecoverySource::CubeInPlace`
    // only when `lease.dirty_verified == Some(true)`, before this prompt is
    // composed. Same-workspace alone is not proof: cube can re-lease the
    // same workspace after resetting it, or the followup can sit `ready`
    // long enough (pool saturation, dependency gating) for an unrelated
    // task to lease, dirty, and release that workspace first — in which
    // case `--allow-dirty` hands this worker a foreign working copy, not
    // its own cancelled review draft.
    let verified_in_place = current == Some(preferred)
        && boss_engine_recovery::recovery_apply::RecoveryReport::read_for(workspace_path, &execution.id)
            .is_some_and(|report| report.source == boss_engine_recovery::recovery_apply::RecoverySource::CubeInPlace);

    let mut block = String::from("## MERGE-CANCELLED REVIEW RECOVERY\n\n");
    if verified_in_place {
        block.push_str(&format!(
            "This followup was created after {origin} merged while its review-revision worker was mid-run. \
             The engine re-leased that worker's exact workspace (`{preferred}`) without resetting it. \
             Its working copy remains on the merged PR's revision base and may contain partial, \
             uncommitted edits from the cancelled turn.\n\n\
             Inspect before changing the checkout:\n\n\
             ```\n\
             jj status\n\
             jj diff --stat\n\
             jj diff\n\
             ```\n\n\
             Do not trust or discard those edits. They were cut off mid-turn and were never compiled or \
             tested. Reconcile them against this followup and current `main`, then run the required \
             validation before opening the fresh PR.\n\n",
        ));
    } else if current == Some(preferred) {
        block.push_str(&format!(
            "This followup was created after {origin} merged while its review-revision worker was mid-run, \
             and the engine re-leased that worker's exact workspace (`{preferred}`). However, the engine \
             has no confirmed record of what this lease actually returned: it may have been reset (no \
             edits present), or it may have been leased and dirtied by an unrelated task in between and \
             then released back to the pool before landing here. Do not assume the working copy is your \
             own cancelled review draft either way.\n\n\
             Check before doing anything else:\n\n\
             ```\n\
             jj status\n\
             jj diff --stat\n\
             ```\n\n\
             If it holds nothing, proceed as a fresh start from current `main`. If it holds edits, verify \
             they actually belong to this followup's own history (check the log against {origin}'s revision \
             base) before building on them — an edit set from an unrelated task must not be folded into \
             this PR.\n\n",
        ));
    } else {
        let current = current.unwrap_or("an unrecorded fallback workspace");
        block.push_str(&format!(
            "This followup was created after {origin} merged while its review-revision worker was mid-run. \
             The engine preferred the cancelled worker's workspace (`{preferred}`), but cube could not \
             lease it and dispatched this execution on `{current}` instead. This is a fresh-workspace \
             fallback: no partial edits were inherited here. An unverified draft may still remain in \
             `{preferred}` on the merged PR's base; proceed from current `main` in this workspace and do \
             not assume that draft was validated or delivered.\n\n",
        ));
    }
    Some(block)
}

/// The structured-output payload this execution's prompt is built around —
/// the one `$BOSS_STRUCTURED_OUTPUT` names.
///
/// Each execution kind designates at most one: a reviewer produces a
/// `ReviewResult`, a triage agent a decision, a design-postmortem worker its
/// required uncompleted-work manifest, an implementer its optional followups.
/// The PR URL is deliberately *not* here — an implementer produces both, so it
/// gets its own path and env var (see [`structured_output_env_vars`]).
/// Returns `None` for kinds with no designated payload (answer agent, CI
/// remediation, plain design tasks).
pub(super) fn designated_output_kind(execution: &WorkExecution, work_item: &WorkItem) -> Option<StructuredOutputKind> {
    match execution.kind {
        ExecutionKind::PrReview => Some(StructuredOutputKind::ReviewResult),
        ExecutionKind::AutomationTriage => Some(StructuredOutputKind::TriageDecision),
        // A `design_postmortem` task reuses `ProjectDesign` for dispatch, so
        // the payload is chosen by the task's own kind — matching the branch
        // in `compose_execution_prompt` that renders its directive.
        ExecutionKind::ProjectDesign => matches!(
            work_item,
            WorkItem::Task(t) | WorkItem::Chore(t) if t.kind == TaskKind::DesignPostmortem
        )
        .then_some(StructuredOutputKind::PostmortemFollowups),
        // Kept in lockstep with the `followups_emission_block` condition in
        // `compose_execution_prompt`: these are the kinds whose prompt carries
        // the followups instruction.
        ExecutionKind::TaskImplementation
        | ExecutionKind::ChoreImplementation
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::RevisionImplementation => Some(StructuredOutputKind::Followups),
        _ => None,
    }
}

/// Worker env carrying the structured-output artifact paths: the designated
/// payload's path as `$BOSS_STRUCTURED_OUTPUT` (when the kind has one) and the
/// PR-URL artifact's path as `$BOSS_PR_URL_OUTPUT`.
///
/// Built through [`crate::driver::default_structured_output_wiring`] so the
/// env-file contract has a single source of truth with the driver's
/// [`crate::driver::AgentDriver::structured_output_wiring`] default. The
/// operative instruction is always the literal path embedded in the prompt —
/// the worker writes the file directly, not by expanding env vars — but
/// exporting them keeps the convention self-documenting in the pane and lets
/// a script resolve it.
pub(super) fn structured_output_env_vars(
    dir: &Path,
    execution: &WorkExecution,
    work_item: &WorkItem,
) -> Vec<(String, String)> {
    use crate::driver::{StructuredOutputRequest, default_structured_output_wiring};

    let mut env = Vec::new();
    let pr_path = crate::structured_output::path_for(dir, &execution.id, StructuredOutputKind::PrUrl);
    env.extend(
        default_structured_output_wiring(&StructuredOutputRequest {
            kind: StructuredOutputKind::PrUrl,
            result_path: &pr_path,
            schema: None,
        })
        .env,
    );
    if let Some(kind) = designated_output_kind(execution, work_item) {
        let path = crate::structured_output::path_for(dir, &execution.id, kind);
        env.extend(
            default_structured_output_wiring(&StructuredOutputRequest {
                kind,
                result_path: &path,
                schema: None,
            })
            .env,
        );
    }
    env
}

pub(super) fn compose_execution_prompt(params: ExecutionPromptParams<'_>) -> String {
    let ExecutionPromptParams {
        execution,
        work_item,
        parent_project,
        workspace_path,
        cube_change_id,
        conflict_attempt,
        ci_attempt,
        editorial_rules,
        design_guidance,
        pr_template_set,
        editorial_enabled,
        worker_signal_proposals_seam_enabled,
        deferred_scope_proposals_seam_enabled,
        followup_proposals_seam_enabled,
        run_done_proposals_seam_enabled,
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
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    let mut prompt = String::new();
    prompt.push_str("You are a reusable Boss worker running one execution inside a dedicated repo workspace.\n");
    prompt.push_str("The current session cwd is already set to that workspace.\n");
    prompt.push_str("Do the work directly in the repository checkout before ending this run.\n");
    prompt.push_str("Avoid asking the human for permission during this pass; when you need review or direction, stop and summarize it clearly.\n\n");

    // If the chore already has a PR, inject a high-prominence resume
    // directive BEFORE the execution context so it outweighs the
    // workspace-rules default of `jj git fetch && jj new main`.
    let existing_pr_url = work_item_pr_url(work_item);
    if let Some(block) = merge_cancelled_review_recovery_block(execution, work_item, workspace_path) {
        prompt.push_str(&block);
    } else if let Some(pr_url) = existing_pr_url {
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
             {cube} workspace goto --pr {pr_number}   # lands you on the PR branch\n\
             ```\n\
             Then make your changes on that branch and push:\n\
             ```\n\
             {cube} pr update --branch <branch-name>\n\
             ```\n\
             \n\
             If the branch cannot be resumed (deleted upstream, conflict you cannot resolve, etc.),\n\
             STOP and surface the blocker — do NOT silently open a parallel PR.\n\n",
        ));
    } else if let Some(report) =
        boss_engine_recovery::recovery_apply::RecoveryReport::read_for(workspace_path, &execution.id)
    {
        // No PR URL on the work item, but the engine holds a durable pointer
        // that it recovered this respawn's state (a marker it wrote itself —
        // see `startup_recovery_block`'s doc comment). Absent that marker,
        // this is a fresh dispatch like any other: the ordinary "expected
        // branch name" / `jj new main` guidance further down is the correct,
        // honest instruction, so no block is rendered at all.
        prompt.push_str(&startup_recovery_block(&report));
    } else if execution.allow_dirty {
        // No recovery marker, but the engine recorded this as a dirty
        // re-lease (see `reconcile_workspace_recovery`): cube handed the
        // workspace back without a reset, yet no marker was written — e.g.
        // `dirty_verified` was `None` and no patch was captured, or writing
        // the marker itself failed. Either way `@` may already hold a prior
        // worker's uncommitted edits, so the ordinary `jj new main` guidance
        // further down would silently discard them if followed blind.
        prompt.push_str(
            "## WORKSPACE RE-LEASED WITHOUT A RESET\n\n\
             This workspace was re-leased without a reset. Run `jj status` before `jj new \
             main` and keep anything you find — it may hold a prior worker's uncommitted \
             edits.\n\n",
        );
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
                    &crate::structured_output::default_path_string(
                        &execution.id,
                        StructuredOutputKind::PostmortemFollowups,
                    ),
                    design_guidance,
                ));
            } else {
                prompt.push_str(&compose_design_directive(parent_project, design_guidance));
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
                (
                    worker_signal_proposals_seam_enabled,
                    deferred_scope_proposals_seam_enabled,
                ),
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
        // PRIMARY channel for the PR URL: an engine-owned artifact file the
        // worker writes (design: agent-driver abstraction §1.6 — file-based
        // StructuredOutput). Driver-agnostic: no transcript convention, no
        // hook stream, just a path. The final-message line below stays as the
        // Claude driver's fallback producer, and the `PostToolUse` capture of
        // `gh pr create` stdout remains as its hook-derived one.
        let pr_url_artifact = crate::structured_output::default_path_string(&execution.id, StructuredOutputKind::PrUrl);
        if let Some(pr_url) = existing_pr_url {
            let pr_number = boss_github::pr_url::pr_number_from_url(pr_url)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            prompt.push_str(&format!(
                "\nAcceptance criterion: when you believe the work is done, the deliverable is a PR URL.\n\
                 - Push your commits to the existing PR branch with `{cube} pr update --branch <branch-name>` (see the ## RESUME EXISTING PR block above). Do NOT open a new PR.\n\
                 - Confirm the PR is updated with `gh pr view {pr_number}` (pass `-R owner/repo` since bare gh calls need it in a jj workspace — use `jj git remote` to find the slug, or check the PR URL above).\n\
                 - As soon as cube prints the PR URL, record it by writing the file `{pr_url_artifact}` with the contents `{{\"pr_url\": \"<the url>\"}}` (path also exported as `$BOSS_PR_URL_OUTPUT`). That path is outside the repo/workspace, so it never pollutes your PR, and it is the channel the engine reads first.\n\
                 - Print the PR URL on its own line as the final thing in your final response as well, so the engine can still pick it up if that file write fails.\n\
                 - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, you have made no changes — do NOT commit, push, or open a PR. Stop and explain what went wrong instead.\n",
            ));
        } else {
            prompt.push_str(&format!(
                "\nAcceptance criterion: when you believe the work is done, the deliverable is a PR URL.\n\
                 - Use the engine-supplied branch name from the `expected branch name` line above (`{expected_branch}`) when creating your bookmark — do NOT invent a different name.\n\
                 - Push your branch (`jj bookmark create {expected_branch} -r @`) and open a PR with `{cube} pr create --branch {expected_branch}` which pushes the branch and opens the PR in one step (jj-aware, no GIT_DIR needed). It is safe to retry: if a prior call already created the PR (e.g. your tool killed an earlier invocation on a timeout but the push had actually landed), it returns that PR's URL instead of erroring. Use `{cube} pr update --branch {expected_branch}` only when you have new commits to push onto an already-open PR.\n\
                 - **Never use `jj git push`, `git push`, or `gh pr create` directly** — always use `{cube} pr create` or `{cube} pr update`. A PreToolUse hook blocks direct push/PR-create attempts and redirects you to cube.\n\
                 - If a PR already exists for this branch (e.g. you are resuming work or addressing review comments), push your new commits to update it instead of opening a duplicate. Check with `gh pr view` from inside the workspace.\n\
                 - As soon as cube prints the PR URL, record it by writing the file `{pr_url_artifact}` with the contents `{{\"pr_url\": \"<the url>\"}}` (path also exported as `$BOSS_PR_URL_OUTPUT`). That path is outside the repo/workspace, so it never pollutes your PR, and it is the channel the engine reads first.\n\
                 - Print the PR URL on its own line as the final thing in your final response as well, so the engine can still pick it up if that file write fails.\n\
                 - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, you have made no changes — do NOT commit, push, or open a PR. Stop and explain what went wrong instead.\n",
            ));
        }
        // Warn that PR creation is terminal — the engine reaps the worker
        // immediately after the PR is opened. Workers must finish everything
        // BEFORE opening the PR; no followup turn is possible.
        prompt.push_str(&pr_terminal_directive(run_done_proposals_seam_enabled));
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
            prompt.push_str(&deferred_scope_directive(deferred_scope_proposals_seam_enabled));
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
            &crate::structured_output::default_path_string(&execution.id, StructuredOutputKind::Followups),
            followup_proposals_seam_enabled,
        ));
    }
    // Terminal run-done declaration — emitted for EVERY execution kind,
    // unlike every other seam directive above. The kinds that never open a
    // PR (reviews, triage, conflict resolutions, revisions) are exactly the
    // ones whose ending the engine could otherwise only infer, so scoping
    // this to PR-producing kinds would miss the cases it exists for.
    prompt.push_str(&run_done_directive(run_done_proposals_seam_enabled));
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
/// failure at `"$BOSS_BIN" propose blocked`; `false` reproduces the pre-migration
/// `[blocked]` marker sentence, so a worker on the flag-off path is never
/// told to call a verb the engine won't yet honor proposals-first.
pub(crate) fn bazel_prepush_gate_text(seam_enabled: bool) -> String {
    let failure_sentence = if seam_enabled {
        "If the build or tests actually fail or actually time out — a real command that ran and returned a failing or timed-out result — do NOT push red code and do NOT idle waiting on them. Deciding on your own that the run has gone on long enough is not a failure or a timeout, and is never a reason to stop short of a clean result. A command still producing output is slow, not wedged — wait for it. A command producing no output and no progress is wedged: re-run it wrapped in an explicit `timeout` so it returns a real result you can act on, rather than waiting on it or guessing. Call `\"$BOSS_BIN\" propose blocked --reason \"...\"` naming the failing/timed-out command and its output, and stop (see \"If you are blocked or the work is bigger than estimated\" below for the exact syntax). Escalating a blocker is correct; pushing a known-broken branch — or hanging on a wedged build — is not.\n"
    } else {
        "If the build or tests actually fail or actually time out — a real command that ran and returned a failing or timed-out result — do NOT push red code and do NOT idle waiting on them. Deciding on your own that the run has gone on long enough is not a failure or a timeout, and is never a reason to stop short of a clean result. A command still producing output is slow, not wedged — wait for it. A command producing no output and no progress is wedged: re-run it wrapped in an explicit `timeout` so it returns a real result you can act on, rather than waiting on it or guessing. Emit a `[blocked] reason=\"...\"` marker in your final response naming the failing/timed-out command and its output, and stop (see \"If you are blocked or the work is bigger than estimated\" below for the exact syntax). Escalating a blocker is correct; pushing a known-broken branch — or hanging on a wedged build — is not.\n"
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
         - Both `bazel build` and `bazel test` must finish clean — exit 0, no build errors, no failing tests — before you push. Clippy/lint is not reported by bazel; it runs at push time via the checkleft guard (and in CI).\n\
         \n\
         Run every long-running build-class command (Bazel, checkleft, tests, etc.) in the FOREGROUND and read its exit code directly. For a tool that yields a session, FOREGROUND means keep polling that session until it returns `exit_code`; it does not mean issue one invocation and discard its session handle. Do NOT background one with a trailing shell `&` or a backgrounded/asynchronous invocation and then idle in a self-paced wait-loop \"until the gate is green\". If the command wedges (host contention, a hung toolchain), a self-paced wait-loop never terminates and you strand your slot. If you need an upper bound, wrap the command itself in a timeout (e.g. `timeout 1800 bazel test //...`) so it returns control to you on expiry; on a timeout, treat it as a blocker (below), do not retry-and-idle. To diagnose a command, inspect only this invocation's output base and logs; never infer ownership or blockage from global process-name matches.\n\
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
        "If `bazel build` fails (the merge does not compile) and you cannot make it compile, do NOT push. Deciding on your own that the run has gone on long enough is not a build failure, and is never a reason to stop short of a clean build. Fix the resolution, or — if it needs a human decision — follow the stop conditions below. Do NOT idle waiting on a wedged build; call `\"$BOSS_BIN\" propose blocked --reason \"...\"` naming the failure and stop.\n"
    } else {
        "If `bazel build` fails (the merge does not compile) and you cannot make it compile, do NOT push. Deciding on your own that the run has gone on long enough is not a build failure, and is never a reason to stop short of a clean build. Fix the resolution, or — if it needs a human decision — follow the stop conditions below. Do NOT idle waiting on a wedged build; emit a `[blocked] reason=\"...\"` marker naming the failure and stop.\n"
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
/// `seam_enabled` mirrors `run_done_proposals_seam`. When on, this block
/// gains one sentence resolving what would otherwise be a direct
/// contradiction between two directives: this one says PR creation is the
/// last thing you do, and [`run_done_directive`] asks for a declaration.
/// Both are true — the declaration goes immediately BEFORE the push, since
/// the push is what may reap the worker — but a worker left to reconcile
/// them itself will reasonably conclude it cannot do both, and drop one.
fn pr_terminal_directive(seam_enabled: bool) -> String {
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
    let mut out = String::new();
    out.push_str("\n## Important: PR creation is your terminal act\n\n");
    out.push_str(
        "Opening the PR is the LAST thing you do. The engine reaps you immediately after the PR is created.\n\n",
    );
    if seam_enabled {
        out.push_str(
            "The one thing that comes after everything else and BEFORE the push is your run-done \
             declaration (`boss propose done`, see below) — submit it, then open/update the PR. \
             Doing it in that order is deliberate: the push can reap you, so a declaration you \
             planned to make afterwards may never happen.\n\n",
        );
    }
    out.push_str(&format!("You will NOT get another turn after `gh pr create` / `{cube} pr create` (or `{cube} pr update` for an existing PR). Do not plan followup commits, do not defer work to \"after the PR\", do not open the PR while background work (parallel/sub-agent runs, backgrounded builds, code reviews) is still in flight expecting to consume its results.\n\n"));
    out.push_str("Therefore: finish everything — including consuming any review/self-review findings you started — BEFORE you open the PR. If a background review is still running and you care about its results, wait for it and address all findings FIRST, then open the PR. If you don't intend to wait, don't start the review.\n");
    out
}

/// Worker Stop-boundary escalation/blocker protocol directive. `seam_enabled`
/// mirrors the `worker_signal_proposals_seam` feature flag — the same flag
/// [`crate::completion::WorkerCompletionHandler::detect_and_file_worker_signals`]
/// reads for the engine's read path, threaded here so the two halves of the
/// migration move together: a worker must never be taught a verb the engine
/// won't yet read proposals-first for, and flipping the flag off must
/// restore the marker-only vocabulary on the prompt side, not just the
/// engine's read side.
///
/// `seam_enabled = true` documents the two sanctioned `"$BOSS_BIN" propose` verbs a
/// worker calls when it cannot proceed unassisted: `effort-escalation` (the
/// work is bigger than estimated) and `blocked` (a human/coordinator
/// decision is needed), plus the `[blocked]` marker retained as a bootstrap
/// fallback of last resort. `"$BOSS_BIN" propose` validates synchronously, so a
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
/// `seam_enabled = false` renders the marker-grammar variant of this
/// directive: both `[effort-escalation]` and `[blocked]` as markers, no
/// `"$BOSS_BIN" propose` mention anywhere. Shared guidance that names no verb
/// (e.g. what makes a reason valid) is carried by both branches. Incident
/// 2026-07-02 (`exec_18b5243e65ff188_2d`) is why the marker syntax is
/// spelled out explicitly rather than left implicit — a worker hit a
/// bazel blocker it could not resolve, did the right thing by stopping
/// instead of pushing broken code, and emitted a bare
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
     `<why>` must name an external fact — a command that ran and failed (with its output), a \
     missing credential, an instruction that genuinely conflicts with another. Citing the run's \
     own duration, your own context usage, your own decision to stop, or that a required step \
     \"was not completed\" is not a valid reason: none of those are blockers, they are you \
     choosing to stop, and this marker is not a channel for that.\n\n\
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
     - **`\"$BOSS_BIN\" propose effort-escalation --level <level> --reason \"<why>\"`** — the assigned work \
     needs more effort than it was classified at. `<level>` is one of \
     `trivial|small|medium|large|max`. Example:\n\n\
     ```\n\
     \"$BOSS_BIN\" propose effort-escalation --level large --reason \"ran into a multi-subsystem race; description didn't mention the engine/app boundary\"\n\
     ```\n\n\
     - **`\"$BOSS_BIN\" propose blocked --reason \"<why>\"`** — you cannot proceed without a \
     human/coordinator decision: a build failure you can't resolve, an ambiguous requirement, \
     conflicting instructions, a missing credential. Example:\n\n\
     ```\n\
     \"$BOSS_BIN\" propose blocked --reason \"bazel build fails with E0583 for a newly added file, survives clean --expunge; need guidance or explicit direction before proceeding\"\n\
     ```\n\n\
     `--reason` must name an external fact — a command that ran and failed (with its output), a \
     missing credential, an instruction that genuinely conflicts with another. Citing the run's \
     own duration, your own context usage, your own decision to stop, or that a required step \
     \"was not completed\" is not a valid reason: none of those are blockers, they are you \
     choosing to stop, and this call is not a channel for that.\n\n\
     Either call files a coordinator-visible attention item immediately and pauses the \"produce a \
     PR\" auto-nudge loop for this run until a coordinator acks it, so you will not be re-prompted \
     to \"produce a PR\" while one is pending. Do NOT stop silently or push code you know is broken \
     to work around a blocker: call `\"$BOSS_BIN\" propose` instead.\n\n\
     **Bootstrap fallback only:** if `\"$BOSS_BIN\" propose` itself is unreachable (the mechanism is down, \
     the socket is gone, or you are a remote worker with no local peer to attribute the call to), \
     fall back to a bare `[blocked] reason=\"<why>\"` line on its own line in your final response — \
     the one marker kept specifically because it must still work when the mechanism itself is \
     broken. If the underlying problem is an effort escalation rather than a blocker, state the \
     requested level in the reason (e.g. `[blocked] reason=\"boss propose unreachable; requesting \
     effort escalation to large — <why>\"`) — this bootstrap channel is the only one guaranteed to \
     work when `\"$BOSS_BIN\" propose` itself is down, so it carries both signal kinds rather than teaching \
     a second marker grammar back. Do not use it once `\"$BOSS_BIN\" propose` has already succeeded for this \
     signal; it is a last resort, not a second channel.\n"
        .to_string()
}

/// Terminal run-done declaration directive — the worker-facing half of the
/// `run_done_proposals_seam` migration.
///
/// `seam_enabled` mirrors the feature flag the engine's read path reads
/// (see [`crate::completion::WorkerCompletionHandler::evaluate_satisfied_deliverable_on_stop`]
/// and [`crate::run_done_backstop`]), threaded here so the two halves move
/// together: a worker must never be taught a verb the engine won't act on,
/// and flipping the flag off must restore today's prompt exactly. With the
/// seam off this contributes nothing at all.
///
/// Unlike every other seam directive, this one is emitted for **every**
/// execution kind. That is the requirement it exists to meet: revisions, CI
/// fixes, conflict resolutions and reviewer passes all terminate without
/// creating anything the engine can point at, so they are precisely the runs
/// whose ending was previously guessed at. A directive scoped to
/// PR-producing kinds would leave the failing cases uncovered.
///
/// Two things the wording works hard at, both learned from the incidents
/// this closes:
///
/// - **When to declare.** Opening or updating a PR can reap the worker
///   immediately, so "declare afterwards" is advice a worker cannot follow.
///   The declaration goes immediately *before* the terminal push.
/// - **Not declaring is not a shortcut to being left alone.** A worker that
///   reads "the engine waits for my declaration" as "so I can just stop"
///   would swap one silent failure for another. The directive states the
///   real consequence: the run is held, then asked, then parked for a human
///   — visibly unresolved, never quietly successful.
pub(crate) fn run_done_directive(seam_enabled: bool) -> String {
    if !seam_enabled {
        return String::new();
    }
    "\n## Declaring your run finished\n\n\
     When your run is over, say so:\n\n\
     ```\n\
     boss propose done --outcome <delivered|no-changes-needed|blocked> --summary \"<one line>\"\n\
     ```\n\n\
     This is what ends your run. The engine does not decide it from the state of your PR — for a \
     run dispatched against a PR that is already open and green, that state says nothing about \
     whether you did anything, and reading it as \"finished\" is how mid-investigation runs used to \
     get terminated with their work lost.\n\n\
     Pick the outcome that is true:\n\n\
     - `delivered` — the deliverable exists (you opened or pushed to the PR, wrote the review, \
     posted the reply).\n\
     - `no-changes-needed` — you verified there was nothing to produce. This replaces the \
     `NO_CHANGES_NEEDED` marker; you do not need both.\n\
     - `blocked` — you are stopping without delivering. File `boss propose blocked --reason \"...\"` \
     alongside it so the blocker itself is recorded, not just the fact that you stopped.\n\n\
     **Declare immediately BEFORE your terminal push**, not after: `cube pr create` / `cube pr \
     update` can reap you the moment the PR moves, so a declaration you planned to make afterwards \
     may never happen. Declaring first costs nothing if the push then fails — re-declare with the \
     accurate outcome, the newest declaration wins.\n\n\
     If you simply stop without declaring, you are not left alone: the engine holds the run open \
     while it can see you working, then asks you once whether you are finished, then parks the run \
     for a human with the outcome recorded as unknown. That is worse for you and for the human than \
     one command.\n"
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
///
/// `seam_enabled` mirrors the `deferred_scope_proposals_seam` feature flag —
/// the same flag
/// [`crate::completion::WorkerCompletionHandler::detect_and_record_deferred_scope`]
/// reads for the engine's read path, threaded here so the two halves of the
/// migration move together (design implementation task 9, following the
/// recipe [`worker_escalation_protocol_directive`] established): a worker
/// must never be taught the `"$BOSS_BIN" propose deferred-scope` verb when the
/// engine won't yet read proposals-first for it, and flipping the flag off
/// must restore today's marker-only directive exactly.
///
/// `seam_enabled = false` reproduces the pre-migration directive verbatim.
/// `seam_enabled = true` instructs `"$BOSS_BIN" propose deferred-scope` instead —
/// unlike `[blocked]`, the `[deferred-scope]` marker has no bootstrap-
/// fallback carve-out (design §"Failure semantics": only `[blocked]` is
/// retained indefinitely), so the seam-enabled directive teaches the verb
/// only; the engine still accepts a stray legacy marker as a counted
/// fallback (see [`crate::deferred_scope`]).
pub(crate) fn deferred_scope_directive(seam_enabled: bool) -> String {
    if !seam_enabled {
        return "\n## If you deliver less than the brief asks: declare the gap\n\n\
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
            .to_string();
    }
    "\n## If you deliver less than the brief asks: declare the gap\n\n\
     If you consciously decide to narrow scope — implement part of what was asked and \
     deliberately leave a piece undone (it needs plumbing/data/access this run doesn't have, \
     it's a genuinely separate concern, etc.) rather than doing it — call `\"$BOSS_BIN\" propose \
     deferred-scope` once per deferred item, during the run:\n\n\
     ```\n\
     \"$BOSS_BIN\" propose deferred-scope --summary \"<what you did not deliver>\" --reason \"<why you deferred it>\"\n\
     ```\n\n\
     Both flags are required. Example:\n\n\
     ```\n\
     \"$BOSS_BIN\" propose deferred-scope --summary \"wiring for the third data source\" --reason \"needs a new ingestion pipeline; out of scope for this wiring-only chore\"\n\
     ```\n\n\
     Submission is synchronous and validated immediately — a malformed call fails right away with \
     a typed error you can fix and retry, unlike a marker line the engine only reads long after \
     you've moved on. Do NOT write \"filed as a followup\", \"tracked separately\", or similar in \
     your PR body or summary as a substitute for calling this — you have no other way to file or \
     track anything, that sentence would simply be false. `\"$BOSS_BIN\" propose deferred-scope` is the \
     channel that actually creates a durable record: it is recorded against this task and \
     surfaced to a human, who decides whether to spin up a followup or accept the gap. This is \
     distinct from the followups mechanism above, which proposes brand-new out-of-scope work you \
     noticed — use `\"$BOSS_BIN\" propose deferred-scope` specifically for work the brief asked for that \
     you did not deliver.\n\n\
     **The verb is the only sanctioned channel for declaring deferred scope — prose is not \
     enough.** If your PR body, a summary section, or your final response says anything that \
     states or implies narrowed scope — \"deferred\", \"not included in this PR\", \"left for a \
     future task\", \"out of scope for now\", a \"## Deferred\" heading, or similar — every item \
     it names MUST also have a matching `\"$BOSS_BIN\" propose deferred-scope` call. A prose deferral \
     section with no matching proposal is a protocol violation: reviewers are instructed to flag \
     it, and it will be flagged. A \"## Deferred\" section in the PR body is fine as human-\
     readable prose, but only in addition to the proposal calls, never instead of them.\n"
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
        "call `\"$BOSS_BIN\" propose blocked --reason \"...\"` instead"
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

/// Revision-flavoured counterpart to [`no_op_completion_directive`]. A
/// `revision_implementation` worker is dispatched to address a specific
/// review finding on an already-open PR, not to produce a fresh diff
/// against `main` — so the primary-implementation directive's "if `jj diff`
/// is empty because the work is already on `main`" framing does not apply
/// here, and until this directive existed no revision prompt taught the
/// [`NO_CHANGES_NEEDED`](crate::no_op_signal::NO_CHANGES_NEEDED_MARKER)
/// marker at all: `on_stop_inner`'s revision no-op terminal
/// (`worker_signalled_no_op`) was reachable in the engine but no worker was
/// ever told the marker existed, so a revision that genuinely concluded the
/// finding needed no code change had no honest way to say so — it could
/// only decline and stop, which the Stop-boundary handler then read as "did
/// not contribute" and nudged forever.
///
/// This directive keys the marker on the DISPATCHED FINDING, not on an
/// empty `jj diff`: after actually investigating, if the finding this
/// revision was dispatched for turns out to need no code change (e.g. it
/// was already fixed by a sibling commit, or the finding was itself
/// mistaken), emitting the marker is the sanctioned way to say so. Always
/// appended for every revision, independent of conflict/CI-remediation
/// framing, because the engine's `worker_signalled_no_op` check is itself
/// unconditional on `revision_implementation` executions.
fn revision_no_op_completion_directive(seam_enabled: bool) -> String {
    let marker = crate::no_op_signal::NO_CHANGES_NEEDED_MARKER;
    let blocked_pointer = if seam_enabled {
        "call `\"$BOSS_BIN\" propose blocked --reason \"...\"` instead"
    } else {
        "emit a `[blocked] reason=\"...\"` marker instead"
    };
    let mut out = String::new();
    out.push_str("\n## If the finding needs no code change: signal a sanctioned no-op\n\n");
    out.push_str(
        "This revision was dispatched to address a specific review finding. If, after actually \
         investigating it, you conclude the finding needs NO code change — it was already fixed \
         by another commit, or the finding was itself mistaken — that is a legitimate outcome, but \
         it must be stated explicitly, not just implied by stopping without a push.\n\n",
    );
    out.push_str(&format!(
        "In that case, do NOT push an empty or cosmetic commit to manufacture a diff. Instead, \
         explain in your final response exactly why the finding needs no change, then emit a line \
         containing exactly `{marker}` as the final line of your response, and stop. The engine \
         recognizes this marker (combined with no new commit on the parent PR) as a declared no-op: \
         it closes this revision without a nudge loop, and files a human-visible record that the \
         finding was declined rather than fixed, so a human can judge whether that was right.\n\n",
    ));
    out.push_str(&format!(
        "Do NOT emit `{marker}` to abandon a finding you simply find hard, ambiguous, or are \
         blocked on — if you are blocked, {blocked_pointer} (see \"If you are blocked or the work \
         is bigger than estimated\" above). This marker is specifically for a finding you have \
         determined, after investigation, requires no code change.\n",
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

/// Followups emission instruction (design:
/// `tools/boss/docs/designs/attentions.md`, "Creation pipeline";
/// `worker-proposal-api-replace-fragile-worker-to-engine-seams.md`,
/// implementation task 10). Appended to the implementation-worker directive:
/// a worker that notices concrete, out-of-scope follow-on work near task
/// completion surfaces it for the human.
///
/// `seam_enabled` mirrors the `followup_proposals_seam` feature flag — the
/// same flag `crate::completion::pr_transition`'s followups block reads for
/// the engine's read path, threaded here so the two halves of the migration
/// move together: a worker must never be taught a verb the engine won't yet
/// read proposals-first for, and flipping the flag off must restore today's
/// behavior exactly, prompt included.
///
/// `seam_enabled = false` reproduces the pre-migration directive verbatim:
/// **write** a JSON array to the engine-owned artifact at `output_path` (see
/// [`crate::structured_output`]) — the engine reads + schema-validates that
/// file at completion and upserts a followup group keyed to this task — with
/// a `FOLLOWUPS:` fenced-JSON sentinel in the final message kept as a
/// transitional fallback (and to keep remote workers working until the
/// artifact is fetched cross-host).
///
/// `seam_enabled = true` instructs `"$BOSS_BIN" propose followup-task` instead: one
/// call per follow-up, during the run, not batched into an end-of-run
/// artifact. Submission is synchronous — a malformed call fails right away
/// with a typed error the worker can fix and retry, unlike an artifact/
/// sentinel the engine only reads long after the worker has moved on — and
/// it upserts into the same `followup` attention group visible to the human
/// immediately, not just at completion. Because `followup_task` proposals
/// stay `Gated` (task creation always needs the human batch-accept gesture),
/// the directive carries the same "proposed, not filed" phrasing discipline
/// [`deferred_scope_directive`] teaches: a PR body may reference a proposal
/// only as "proposed follow-up `prp_…`", never "filed as a followup" — the
/// worker has no write path to an actual task, so that claim would be false
/// regardless of which channel emitted it.
fn followups_emission_block(output_path: &str, seam_enabled: bool) -> String {
    if !seam_enabled {
        let mut out = String::new();
        out.push_str("\n## Optional: surface follow-on work as followups\n\n");
        out.push_str(
            "If, while completing this task, you noticed concrete follow-on work worth filing — a separate bug, a needed refactor, a missing test, a docs gap — that is OUT OF SCOPE for this PR, you may surface it for the human. This is OPTIONAL: only include genuine, actionable proposals, never invent work to fill it, and never list the change you just made.\n\n",
        );
        out.push_str(&format!(
            "If (and only if) you have followups, **write** a JSON array of them to this exact file (also exported as `$BOSS_STRUCTURED_OUTPUT`):\n\n`{output_path}`\n\nThis path is outside the repo/workspace, so the manifest never pollutes your PR. Each array element is an object:\n",
        ));
        out.push_str("- `proposed_name` (required): a short task title.\n");
        out.push_str("- `proposed_description` (required): one paragraph of scope.\n");
        out.push_str("- `proposed_effort` (optional): one of `trivial` | `small` | `medium` | `large` | `max`.\n");
        out.push_str("- `proposed_work_kind` (optional): one of `task` | `chore` | `project` (defaults to `task`).\n");
        out.push_str("- `rationale` (optional): why it is worth doing.\n\n");
        out.push_str("File contents example:\n\n```json\n[{\"proposed_name\": \"Add retry/backoff to the X client\", \"proposed_description\": \"The X client fails hard on transient 5xx; add bounded retry with jitter.\", \"proposed_effort\": \"small\", \"proposed_work_kind\": \"task\", \"rationale\": \"Observed flakes during this task.\"}]\n```\n\n");
        out.push_str("Do NOT write the file at all if you have no followups — an absent file means \"no followups\", which is the normal case. Writing it does not block this PR — it just files proposals for the human to review.\n\n");
        out.push_str("As a fallback only (e.g. if the file write is unavailable), you may instead append — after your `## Open Questions` section — a line containing exactly `FOLLOWUPS:` immediately followed by a fenced ```json code block holding the same JSON array.\n");
        return out;
    }
    let mut out = String::new();
    out.push_str("\n## Optional: surface follow-on work as followups\n\n");
    out.push_str(
        "If, while completing this task, you noticed concrete follow-on work worth filing — a separate bug, a needed refactor, a missing test, a docs gap — that is OUT OF SCOPE for this PR, you may surface it for the human. This is OPTIONAL: only include genuine, actionable proposals, never invent work to fill it, and never list the change you just made.\n\n",
    );
    out.push_str(
        "If (and only if) you have followups, call `\"$BOSS_BIN\" propose followup-task` once per follow-up, during the run:\n\n\
         ```\n\
         \"$BOSS_BIN\" propose followup-task --name \"<short task title>\" --description \"<one paragraph of scope>\" --rationale \"<why it is worth doing>\"\n\
         ```\n\n",
    );
    out.push_str("- `--name` (required): a short task title.\n");
    out.push_str("- `--description` / `--description-file` (required, exactly one): one paragraph of scope. Prefer `--description-file` for anything with backticks, quotes, or multiple lines.\n");
    out.push_str("- `--rationale` (required): why it is worth doing.\n");
    out.push_str("- `--effort` (optional): one of `trivial` | `small` | `medium` | `large` | `max`.\n");
    out.push_str("- `--work-kind` (optional): one of `task` | `chore` | `project` (defaults to `chore`).\n\n");
    out.push_str("Example:\n\n```\n\"$BOSS_BIN\" propose followup-task --name \"Add retry/backoff to the X client\" --description \"The X client fails hard on transient 5xx; add bounded retry with jitter.\" --effort small --work-kind task --rationale \"Observed flakes during this task.\"\n```\n\n");
    out.push_str(
        "Do NOT call this at all if you have no followups — never calling it means \"no followups\", which is the normal case. Calling it does not block this PR — it upserts into the originating task's followup group for the human to review; task creation still requires the human's own batch-accept gesture.\n\n",
    );
    out.push_str(&format!(
        "As a fallback only (e.g. if `\"$BOSS_BIN\" propose` is unreachable), you may instead **write** a JSON array to this exact file (also exported as `$BOSS_STRUCTURED_OUTPUT`): `{output_path}`. Each array element is an object with `proposed_name` (required), `proposed_description` (required), `proposed_effort` (optional), `proposed_work_kind` (optional), and `rationale` (optional) — or append a `FOLLOWUPS:` sentinel plus fenced ```json array to your final message. Prefer `\"$BOSS_BIN\" propose followup-task` — it validates synchronously and is visible to the human immediately, instead of waiting until this run completes.\n\n",
    ));
    out.push_str(
        "**Phrasing matters:** a `followup-task` proposal is submitted, not filed — task creation still needs a human's batch-accept gesture. If your PR body or summary references one, say \"proposed follow-up `prp_…`\" (the id the command prints on success), never \"filed as a followup\" or \"tracked separately\" — you have no ability to file or track anything directly, and that phrasing would be false.\n",
    );
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
    out.push_str(&design::doc_structure_conventions_block());
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
    // (worker_signal_proposals_seam_enabled, deferred_scope_proposals_seam_enabled)
    // — bundled to keep the parameter count under clippy::too_many_arguments.
    proposals_seam_flags: (bool, bool),
) -> String {
    let (worker_signal_proposals_seam_enabled, deferred_scope_proposals_seam_enabled) = proposals_seam_flags;
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
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
        out.push_str(&format!("{cube} workspace goto --pr {pr_number}\n"));
        out.push_str("```\n");
    } else {
        out.push_str(
            "**The engine could not determine the PR number from the pr_url field. \
             You MUST position the workspace manually before making any changes \
             (replace `<n>` with the actual PR number):**\n",
        );
        out.push_str("```\n");
        out.push_str(&format!("{cube} workspace goto --pr <n>\n"));
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
    out.push_str(&format!(
        "4. `{cube} pr update --branch <parent-branch-name>`   # pushes to the existing PR; no GIT_DIR or --allow-new needed.\n",
    ));
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
    out.push_str("   f. If, after the comparison in step b, the description already matches the PR's current state, do NOT edit it just to have touched it — leave it as-is and say so explicitly in your final response (e.g. \"PR description verified accurate against this revision's diff; no edit needed\"). The bar is whether the description describes the PR's current state, not whether this revision left a visible mark on it.\n");
    out.push('\n');
    out.push_str("6. **If this revision addresses automated review findings, post a findings-status summary comment on the PR.** This is a DIFFERENT artifact from the PR description in step 5, for a different reader: the description tells reviewers what the PR now does; this comment tells them what happened to each finding from the review pass that spawned this revision.\n");
    out.push_str(
        "   a. This applies ONLY when this task's description (above, in \"What this revision \
         should change\") enumerates review findings — look for one or more `### [<severity>] \
         <title>` sections, the shape an automated review pass renders. If this revision is \
         responding to a plain operator instruction with no such findings, SKIP the comment \
         entirely: do not post an empty table, and state in your final response that the \
         findings-status comment was not applicable.\n",
    );
    out.push_str(
        "   b. Every finding from that review pass must appear as a row, including ones you \
         did NOT change — a table listing only what you fixed defeats the purpose; the reviewer \
         needs to see what was consciously left.\n",
    );
    out.push_str(
        "   c. Post a NEW comment — do not edit or delete a findings-status comment from an \
         earlier revision pass on this PR. Each pass gets its own comment, so the history of \
         prior passes stays visible:\n",
    );
    out.push_str("   ```\n");
    out.push_str("   body=$(mktemp)\n");
    out.push_str("   cat > \"$body\" << 'EOF'\n");
    out.push_str("   ## Review findings — this revision\n");
    out.push_str("   \n");
    out.push_str("   | | Finding | Notes |\n");
    out.push_str("   |---|---|---|\n");
    out.push_str("   | ✅ | <finding title or file:line> | <one sentence: what changed> |\n");
    out.push_str(
        "   | ❌ | <finding title or file:line> | <one sentence: why it was left, or where it went instead> |\n",
    );
    out.push_str("   EOF\n");
    out.push_str(&format!(
        "   gh pr comment {pr_number} -R {repo_slug} --body-file \"$body\"\n"
    ));
    out.push_str("   ```\n");
    out.push_str(
        "   d. Formatting: ✅ or ❌ per row. The \"Finding\" column is a short identifier only \
         — the finding's title, or `file:line` — never the full finding text. The \"Notes\" \
         column is exactly one sentence. A ❌ row's sentence must say WHY the finding was left \
         (or where the issue went instead) — \"not addressed\" alone is not acceptable.\n",
    );
    out.push_str(
        "   e. Never reference a finding by an internal row id in this comment — title or \
         `file:line` only, per this repo's editorial rules against Boss-internal id shapes in \
         worker-authored text.\n",
    );
    out.push_str(
        "   f. This comment does not substitute for the PR description update in step 5 — both \
         are required when findings are present.\n",
    );
    out.push('\n');
    out.push_str(&format!(
        "7. Confirm the new commit is on the PR: `gh pr view {pr_number} -R {repo_slug}`\n"
    ));
    out.push_str(&format!(
        "8. Print the parent PR URL on its own line as the FINAL thing in your final response: {parent_pr_url}\n"
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
    // The Bazel pre-push gate above (both variants) points a build-failure
    // sentence at `"$BOSS_BIN" propose blocked` and, on the non-conflict-resolution
    // variant, at the "If you are blocked or the work is bigger than
    // estimated" section for the exact syntax. Revisions never received that
    // section, leaving the cross-reference dangling and the worker with no
    // `--level` sibling verb or bootstrap-fallback guidance. Push it here so
    // every path that renders the gate also renders what it points at.
    out.push_str(&worker_escalation_protocol_directive(
        worker_signal_proposals_seam_enabled,
    ));
    out.push_str(&deferred_scope_directive(deferred_scope_proposals_seam_enabled));
    out.push_str(&revision_no_op_completion_directive(
        worker_signal_proposals_seam_enabled,
    ));
    out.push_str(&format!(
        "\nAcceptance criterion: when you believe the work is done, the deliverable is the parent PR URL. The post-push steps remain your responsibility: finish them before your final response. The engine treats the driver-resolved boundary after that response as the completion handshake; a push or staged PR URL alone does not mean the revision is done.\n\
         - Push your changes to the parent branch (see step 4 above). Do NOT open a new PR.\n\
         - Update the PR title and description per step 5 above — a stale or contradictory title or description is a defect. If this revision changes or overturns the PR's scope or conclusion, the title MUST reflect the final state.\n\
         - If this revision addressed automated review findings, post the findings-status comment per step 6 above — every finding accounted for, one sentence each.\n\
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
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
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
         decision, not a resolution choice: run `\"$BOSS_BIN\" engine conflicts mark-failed <attempt-id> \
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
    out.push_str(&format!(
        "Run the cube rebase command — it encodes the correct jj recipe automatically \
         and avoids the `@origin` / immutable-heads pitfalls agents commonly hit:\n\n\
         ```\n\
         {cube} workspace rebase\n\
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
    ));
    out.push_str(&format!(
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
         Output must contain no `true`. Only then run `{cube} pr update --branch <branch>`.\n\n\
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
    ));
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
         do NOT push, and run `\"$BOSS_BIN\" engine conflicts mark-failed <attempt-id> --reason <r>`\n\
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
    let cube = boss_engine_worker_bin::WORKER_CUBE_INVOCATION;
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
         {cube} workspace rebase\n\
         ```\n\n\
         Its output (`REBASED_CLEAN` / `REBASED_WITH_CONFLICTS`) is the local ground truth. \
         Quote it in your final response.\n\n\
         **3. You may NOT conclude \"already resolved\" from local `jj` state alone.** \
         `conflicts=false` on some revset, \"the branch is a descendant of `main@origin`\", \
         and `jj git fetch` reporting \"Nothing changed\" are **not** evidence the conflict \
         is gone — a branch can satisfy all three while GitHub still reports `CONFLICTING`. \
         If you believe there is nothing to do, you must show the `gh pr view` output saying \
         `MERGEABLE` and the `{cube} workspace rebase` output saying `REBASED_CLEAN`. Without \
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
             >   queue turn that is semantically incompatible. After fixing, just push and stop —\n\
             >   Boss auto-resubmits the PR to the Trunk queue on the next poller pass, once this\n\
             >   revision comes to rest and the PR's head CI reads green. Do NOT ask a human to\n\
             >   re-run the merge, do NOT comment `/trunk merge` yourself, and do NOT run\n\
             >   `gh pr merge` — that bypasses the queue and races the automatic resubmit.\n\
             > - If a failing construction-branch build is listed below, **push a commit** that \
             >   addresses it — the engine refuses every \"nothing to push\" terminal for a \
             >   queue-side failure because the PR's head CI is green already and so proves nothing \
             >   about that build. If no failing build is listed, do **not** invent one: follow the \
             >   STOP section and `\"$BOSS_BIN\" engine ci mark-failed` rather than force-pushing.\n\n",
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
    out.push_str(&format!(
        "**Head sha at trigger**: `{}`\n",
        crate::work::merge_queue_rebounce_pr_head(&attempt.head_sha_at_trigger),
    ));
    out.push_str(&format!("**Attempt id**: `{}`\n\n", attempt.id));

    out.push_str("### Failing required checks\n\n");
    let captured_checks = render_failed_checks_markdown(&attempt.failed_checks);
    // A Trunk eviction with nothing captured is the shape that destroyed
    // flunge#1137: the engine could not name a failing build, the prompt
    // asserted one existed anyway, and the bail-out below was gated off.
    // Those attempts are now minted (a confirmed queue rejection is
    // authoritative even with no construction build); this STOP section
    // is what keeps the worker from inventing a fix. Merge-queue rebounce
    // *may* land with empty checks when evidence enrichment misses; that
    // path uses the generic directive below rather than the Trunk bail-out.
    let trunk_eviction_without_evidence = is_trunk_eviction && captured_checks.is_none();
    match captured_checks {
        Some(md) => out.push_str(&md),
        None => {
            if is_rebounce {
                let sha_hint = attempt.before_commit_sha.as_deref().unwrap_or("<synthetic-merge-sha>");
                out.push_str(&format!(
                    "_The engine did not capture the failing checks for this merge-queue rebounce. \
                     Do NOT use `gh pr checks` — it shows the PR-head checks, which are green. \
                     Instead, fetch CI for the synthetic merge SHA directly. Prefer legacy commit \
                     statuses when check-runs are empty (Buildkite on mono posts only those): \
                     `gh api repos/<owner>/<repo>/commits/{sha_hint}/status \
                     | jq '.statuses[] | select(.state == \"failure\" or .state == \"error\") | {{context, target_url}}'`; \
                     also try check-runs: \
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
                     filtered client-side to a branch starting with `trunk-merge/pr-{pr_num}/` (do NOT \
                     match `trunk-temp/*` — that is a different, non-gating branch). **A single page — \
                     especially an empty or truncated one — is not proof no such build exists.** Page \
                     with `&page=N` until a page comes back with fewer than `per_page` results, or state \
                     explicitly how many pages you searched, before concluding there is none._\n",
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

    // The bail-out for "the engine cannot name a failing build". It is NOT
    // `mark-noop`: `handle_mark_ci_remediation_noop` rejects every queue-side
    // attempt outright, before it probes, because head-branch CI is green by
    // construction for these and so cannot validate the claim. Pointing a
    // worker at a verb the engine is guaranteed to refuse would be worse than
    // saying nothing. `mark-failed` is the terminal the engine does accept —
    // it is what the rejection message itself recommends.
    if trunk_eviction_without_evidence {
        out.push_str("### If there is no failing build to find (STOP — do not invent one)\n\n");
        out.push_str(&format!(
            "The engine could not identify a failing build for this eviction, and its own \
             classification could not confirm the cause either. Trunk reports the same `failed` state \
             whether a construction build went red, the PR genuinely conflicts with the target branch, \
             or it merely collided with a sibling PR in the same batch — so it is entirely possible \
             **nothing is broken on this PR**, but the engine does not know that, and neither do you \
             yet. Determine the cause yourself, in this order, before deciding what (if anything) to \
             do:\n\n\
             1. **Trunk's newest bot comment** — `gh api repos/<owner>/<repo>/issues/{pr_num}/comments` \
             (paginate if needed) and read the newest `trunk-io[bot]` entry. Its prose names the cause \
             directly: \"...because there was a merge conflict\" is a real conflict against the target \
             branch; \"...because it conflicted with #N\" is a sibling-in-queue collision, not a defect \
             on this PR.\n\
             2. **GitHub's live mergeability** — `gh pr view {pr_num} --json mergeable,mergeStateStatus`, \
             read fresh, not from memory or an earlier command's output. `CONFLICTING` confirms a real \
             conflict. `UNKNOWN` means GitHub is still recomputing — re-run rather than treat it as an \
             answer either way.\n\
             3. **`jj status`** after `jj workspace update-stale` — names conflicted files offline, if \
             your own workspace copy has any.\n\
             4. **The Buildkite search above** — exhaustive (every page, or an explicit \"searched N \
             pages\" claim), not a single possibly-truncated page.\n\n\
             If every one of these says the PR is clean — no conflict, no sibling-in-queue collision, \
             and no failing `trunk-merge/pr-{pr_num}/*` build after an exhaustive search — that is your \
             answer: Trunk never got as far as testing, or the eviction has already cleared. Record it \
             and stop:\n\n\
             ```\n\
             \"$BOSS_BIN\" engine ci classify --attempt-id {attempt} --class unfixable\n\
             \"$BOSS_BIN\" engine ci mark-failed --attempt-id {attempt} --reason no-failing-build-found\n\
             ```\n\n\
             **Do NOT** rebase, reset, force-push, or \"resolve\" anything unless one of the checks \
             above found something concrete on **this PR** for you to act on. A revision whose head \
             ends up with an empty diff has destroyed the PR's contents, not fixed them. If your work \
             would produce a zero-diff commit, stop and mark the attempt failed instead.\n\n",
            pr_num = attempt.pr_number,
            attempt = attempt.id,
        ));
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
             \"$BOSS_BIN\" engine ci mark-noop --attempt-id {attempt} --observed-sha <current-head-sha> --reason already-green\n\
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
             2. Call `\"$BOSS_BIN\" engine ci mark-retriggered --attempt-id <attempt-id> --new-id <new-build-or-run-id>` \
             so the engine records the new run id and stays out of the budget path. Do NOT call \
             `mark-failed` or push code.\n\
             3. Stop. The merge-poller will observe the re-run's outcome on the next sweep.\n\n",
        );
    } else if !trunk_eviction_without_evidence {
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
                "A Trunk queue eviction **whose failing build the engine identified** (listed above) \
                 means something landed on the target branch between this PR's CI run and its queue \
                 turn that is **semantically incompatible**.\n\
                 Fix is: rebase, look at that CI failure on Trunk's construction branch, add a focused \
                 fix, push, and get the PR resubmitted to the queue.\n\n\
                 This does NOT generalise to an eviction with no failing build attached. Trunk reports \
                 the same `failed` state when it could not construct the merge at all, in which case \
                 there is no build, no semantic conflict, and nothing here to fix — see the STOP \
                 section above. Rebasing on that assumption is how a previous run force-pushed an \
                 empty commit over a PR's entire contents.\n\n",
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
                 `\"$BOSS_BIN\" engine ci mark-succeeded-via-rebase --attempt-id <attempt-id>`. The engine \
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
            "1. Classify the failure with `\"$BOSS_BIN\" engine ci classify --attempt-id <attempt-id> --class <tractable|flaky_or_infra|unfixable>`.\n   \
                - `tractable` → there's a clear code change that resolves it. Make it. Push.\n   ",
        );
        // `flaky_or_infra` on a queue-side failure must NOT steer the worker
        // into `mark-retriggered`: that verb is terminal, the engine now
        // rejects it for a queue-side `failure_kind`, and following the old
        // wording is exactly what stranded a real PR out of the Trunk queue
        // for 50 hours (no push, attempt terminal, no resubmit sentinel).
        // Re-running the failing build is still the right diagnosis — the
        // *delivery* is a push, because a resubmit is what actually re-runs
        // the construction build, and only a new head sha triggers one.
        if is_queue_side_failure {
            out.push_str(
                "- `flaky_or_infra` → the failure is environmental. There is still no no-push exit: \
                `mark-retriggered` is rejected for a queue-side failure (it is terminal and would \
                strand the PR out of the queue). Re-running the queue's own build is what a resubmit \
                does, and a resubmit needs a new head sha — so push something that re-triggers it \
                (a rebase onto the current target branch is the usual minimum), or, if the infra \
                failure is genuinely not addressable from this PR, call \
                `\"$BOSS_BIN\" engine ci mark-failed --attempt-id <attempt-id> --reason <reason>` and stop.\n   ",
            );
        } else {
            out.push_str(
                "- `flaky_or_infra` → the failure is environmental. Pivot to the retrigger playbook \
                (re-run the failing build via the provider CLI and call `mark-retriggered`).\n   ",
            );
        }
        out.push_str(
            "- `unfixable` → the failure is real and out of scope. Call \
                `\"$BOSS_BIN\" engine ci mark-failed --attempt-id <attempt-id> --reason <reason>` \
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
                "**Step 3 — push, then stop.**\n\n\
                 The Trunk queue does **not** auto-retry after an eviction, but Boss's own poller does. \
                 The resubmit fires when two things are both true: this revision has come to rest \
                 (its normal terminal is `in_review` — you do NOT need to get it to `done`, and on an \
                 open PR it cannot reach `done`), and the merge-poller sees the PR's head CI green on \
                 your new head sha. Then the poller calls `submitPullRequest` again on its next pass.\n\n\
                 So: push your fix and stop — do NOT ask a human to resubmit, do NOT comment \
                 `/trunk merge` yourself, and do NOT run `gh pr merge`; any of those would race the \
                 automatic resubmit. **Do not exit without pushing**: with no new commit there is no \
                 new head sha, nothing for the poller to observe, and the PR stays out of the queue. \
                 If you truly cannot fix it here, call `\"$BOSS_BIN\" engine ci mark-failed` so a human is \
                 told, rather than stopping silently.\n\n",
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
    prompt.push_str(&format!(
        "**Head sha at trigger**: `{}`\n",
        crate::work::merge_queue_rebounce_pr_head(&attempt.head_sha_at_trigger),
    ));
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
             \"$BOSS_BIN\" engine ci mark-noop --attempt-id {attempt} --observed-sha <current-head-sha> --reason already-green\n\
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
         2. Call `\"$BOSS_BIN\" engine ci mark-retriggered --attempt-id <attempt-id> --new-id <new-build-or-run-id>` \
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
