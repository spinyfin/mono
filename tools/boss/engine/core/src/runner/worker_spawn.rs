//! Worker-spawn composition: [`compose_worker_spawn`] assembles the worker
//! prompt + resolved effort/model config ([`ComposedWorkerSpawn`]), fetching
//! PR review context and diffs for the `pr_review` path.

use std::path::Path;

use boss_engine_gh_invocation::gh_output;

use crate::coordinator::pool_model_override_for_worker_id;
use crate::effort::{SpawnConfig, resolve_spawn_config};
use crate::work::{WorkDb, WorkExecution, WorkItem};
use boss_protocol::ExecutionKind;

use super::prompt::{
    ExecutionPromptParams, compose_answer_agent_prompt, compose_execution_prompt, render_merge_order_preservation_lines,
};
use super::work_item::{work_item_created_via, work_item_name, work_item_pr_url, work_item_task_kind_enum};

/// Composed worker prompt + resolved effort/model config, the output of
/// [`compose_worker_spawn`].
pub(crate) struct ComposedWorkerSpawn {
    pub prompt_text: String,
    pub spawn_config: SpawnConfig,
}

/// Fetch authoritative PR metadata for a reviewer worker's initial prompt.
///
/// Calls `gh pr view <pr_url> --json baseRefOid,headRefOid,files` and returns
/// a [`crate::pr_review::PrReviewContext`] on success. Returns `None` on any
/// network or parse error — callers fall back to the URL-only prompt
/// gracefully without blocking the spawn.
async fn fetch_pr_review_context(pr_url: &str) -> Option<crate::pr_review::PrReviewContext> {
    #[derive(serde::Deserialize)]
    struct PrViewResponse {
        #[serde(rename = "baseRefOid")]
        base_ref_oid: String,
        #[serde(rename = "headRefOid")]
        head_ref_oid: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: String,
        #[serde(default)]
        commits: Vec<PrCommit>,
        #[serde(default)]
        comments: Vec<PrComment>,
    }

    #[derive(serde::Deserialize)]
    struct PrCommit {
        #[serde(rename = "messageHeadline", default)]
        message_headline: String,
        #[serde(rename = "messageBody", default)]
        message_body: String,
    }

    #[derive(serde::Deserialize)]
    struct PrComment {
        #[serde(default)]
        body: String,
    }

    let pr_number = boss_github::pr_url::pr_number_from_url(pr_url)?;

    // Shellout + exit-code/parse boilerplate lives once in
    // `boss_github::pr_files`, shared with `design_detector.rs` and
    // `stacked_pr_structuring.rs`.
    let root =
        boss_github::pr_files::fetch_pr_view_json(pr_url, "baseRefOid,headRefOid,files,title,body,commits,comments")
            .await
            .map_err(|e| {
                tracing::warn!(
                    pr_url,
                    error = %e,
                    "fetch_pr_review_context: gh pr view failed; reviewer will use URL-only prompt",
                );
                e
            })
            .ok()?;

    let changed_files = boss_github::pr_files::parse_changed_file_paths(&root);

    let response: PrViewResponse = serde_json::from_value(root)
        .map_err(|e| {
            tracing::warn!(
                pr_url,
                error = %e,
                "fetch_pr_review_context: failed to parse gh pr view JSON",
            );
            e
        })
        .ok()?;

    // incident-002 P3: deterministically scan the worker's *narrative*
    // surfaces (PR body, commit messages, PR comments) for supersession /
    // obsolescence language. When present, the reviewer is required to verify
    // a design-doc citation for each flagged claim. The diff itself is
    // deliberately excluded ("replace" is ubiquitous in source).
    let mut narrative = String::new();
    narrative.push_str(&response.body);
    narrative.push('\n');
    for c in &response.commits {
        narrative.push_str(&c.message_headline);
        narrative.push('\n');
        narrative.push_str(&c.message_body);
        narrative.push('\n');
    }
    for c in &response.comments {
        narrative.push_str(&c.body);
        narrative.push('\n');
    }
    let supersession_flags =
        crate::supersession_scan::hit_lines(&crate::supersession_scan::scan_supersession_language(&narrative));

    // Mechanical assist for the agent-isms "Boss-construct references"
    // sub-rule: deterministically sweep the PR's own title and description
    // (not commits/comments, which are not part of the sub-rule's scope) for
    // bare T<n>/P<n> tokens. The diff-added-lines half of the sweep is filled
    // in by the caller once the diff is fetched (see `compose_worker_spawn`).
    let mut boss_construct_refs = crate::boss_construct_scan::hit_lines(
        &crate::boss_construct_scan::scan_narrative_text(&response.title, "PR title"),
    );
    boss_construct_refs.extend(crate::boss_construct_scan::hit_lines(
        &crate::boss_construct_scan::scan_narrative_text(&response.body, "PR description"),
    ));

    Some(crate::pr_review::PrReviewContext {
        pr_number,
        base_sha: response.base_ref_oid,
        head_sha: response.head_ref_oid,
        changed_files,
        diff_content: None,
        // Filled in by the caller, which has the `WorkDb` handle needed to
        // resolve the review-cycle root for a revision-triggered pass.
        last_reviewed_sha: None,
        supersession_flags,
        // Filled in by the caller (the spawn path computes the merge-parent
        // deletion tripwire for conflict-resolution reviews — incident-002 P2).
        merged_parent_deletions: Vec::new(),
        boss_construct_refs,
    })
}

/// Fetch the raw diff for a PR via `gh pr diff <pr_url>`.
///
/// Returns the full diff text on success. Returns `None` on any error —
/// callers fall back gracefully (reviewer fetches the diff itself). The
/// caller is responsible for deciding whether the diff is small enough to
/// embed.
async fn fetch_pr_diff(pr_url: &str) -> Option<String> {
    let output = gh_output(&["pr", "diff", pr_url]).await.ok()?;

    if !output.status.success() {
        tracing::warn!(
            pr_url,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "fetch_pr_diff: gh pr diff failed; reviewer will fetch diff itself",
        );
        return None;
    }

    String::from_utf8(output.stdout)
        .map_err(|e| {
            tracing::warn!(
                pr_url,
                error = %e,
                "fetch_pr_diff: diff output is not valid UTF-8",
            );
            e
        })
        .ok()
}

/// Per-execution prompt + spawn-config composition shared by every
/// worker transport.
///
/// [`PaneSpawnRunner`] (local libghostty panes) and
/// [`crate::host_adapter::SshHostAdapter`] (remote SSH workers) both call
/// this so the two launch paths hand the worker a byte-identical prompt
/// and resolve the same effort/model knobs (design §Q3). It gathers the
/// per-execution collaborator context (parent project, merge-conflict /
/// CI-remediation attempt, crash-recovery branch, automation-triage
/// preamble), composes the prompt via [`compose_execution_prompt`], then
/// prepends the effort addendum and the product dispatch preamble exactly
/// as the local runner historically did.
///
/// Transport-agnostic: it reads only from `work_db` (and, for `pr_review`
/// executions, calls `gh pr view` to pre-fetch the PR metadata for the
/// reviewer's initial prompt).
pub(crate) async fn compose_worker_spawn(
    work_db: &WorkDb,
    worker_id: &str,
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    cube_change_id: Option<&str>,
    // (editorial_enabled, max_embed_diff_lines, worker_signal_proposals_seam_enabled)
    // — bundled to keep the parameter count under clippy::too_many_arguments.
    editorial_opts: (bool, u64, bool),
) -> anyhow::Result<ComposedWorkerSpawn> {
    let (editorial_enabled, max_embed_diff_lines, worker_signal_proposals_seam_enabled) = editorial_opts;
    // For any project-scoped task (the synthetic `kind = 'design'`
    // task and ordinary `project_task` rows alike), the richer
    // brief — what the project is for, what its goal is — lives
    // on the parent project rather than on the task row. Look it
    // up at spawn time so the worker prompt is always anchored on
    // the current project state, not whatever was copied at
    // create time.
    let parent_project = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task
            .project_id
            .as_deref()
            .and_then(|project_id| work_db.get_project(project_id).ok()),
        _ => None,
    };
    // For revision_implementation executions with a merge-conflict
    // provenance, look up the linked attempt by the id embedded in
    // created_via (format: "merge-conflict:<crz_id>") so
    // compose_revision_directive can inject the conflict fragment.
    let conflict_attempt = if execution.kind == ExecutionKind::RevisionImplementation {
        work_item_created_via(work_item)
            .and_then(|cv| cv.strip_prefix("merge-conflict:"))
            .and_then(|id| work_db.get_conflict_resolution(id).ok().flatten())
    } else {
        None
    };
    // merge_order forward-port stamping (direction 2): when this is a
    // conflict-resolution revision whose parent has a `merge_order` sibling
    // that already merged, name that sibling in the brief so the worker
    // preserves its surfaces. Keyed on the review-cycle root (the in-review
    // parent), since the merge_order edge is on the original sibling task, not
    // this revision row. Fail-open: a lookup error just omits the clause (the
    // generic preservation rule + the deletion tripwire still apply).
    let merge_order_preservation: Vec<String> = if conflict_attempt.is_some() {
        let root = work_db.review_cycle_root_id(&execution.work_item_id);
        match work_db.merge_order_merged_siblings(&root) {
            Ok(siblings) => render_merge_order_preservation_lines(&siblings),
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    error = %format!("{err:#}"),
                    "merge_order: merged-sibling lookup failed for forward-port brief; omitting sibling clause",
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    // Detect whether this is a respawn after a crash: if the work item has
    // no task-level pr_url (handled by the existing RESUME EXISTING PR path)
    // but has a prior orphaned execution with no pr_url, derive its expected
    // branch so the new worker can attempt to resume it.
    let recovery_branch: Option<String> = if work_item_pr_url(work_item).is_none() {
        match work_db.get_prior_orphaned_execution(&execution.work_item_id, &execution.id) {
            Ok(Some(prior)) => {
                let branch = crate::completion::expected_branch_name(
                    &prior.id,
                    &prior.branch_naming,
                    prior.worker_branch_prefix.as_deref(),
                );
                tracing::info!(
                    execution_id = %execution.id,
                    prior_execution_id = %prior.id,
                    recovery_branch = %branch,
                    "startup recovery: prior orphaned execution found; directing worker to attempt branch resume",
                );
                Some(branch)
            }
            Ok(None) => {
                tracing::debug!(
                    execution_id = %execution.id,
                    "startup recovery: no prior orphaned execution found; worker will start from main",
                );
                None
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    error = %format!("{err:#}"),
                    "startup recovery: failed to query prior orphaned execution; worker will start from main",
                );
                None
            }
        }
    } else {
        None
    };

    // For ci_remediation executions (retrigger-kind only after Phase 5),
    // look up the active attempt so the prompt can show the failing checks.
    //
    // For revision_implementation executions with a ci-fix provenance,
    // look up the linked attempt by the id embedded in created_via
    // (format: "ci-fix:<crm_id>") so compose_revision_directive can
    // inject the CI remediation fragment.
    let ci_attempt = if execution.kind == ExecutionKind::CiRemediation {
        work_db
            .active_ci_remediation_for_work_item(&execution.work_item_id)
            .ok()
            .flatten()
    } else if execution.kind == ExecutionKind::RevisionImplementation {
        work_item_created_via(work_item)
            .and_then(|cv| cv.strip_prefix("ci-fix:"))
            .and_then(|id| work_db.get_ci_remediation(id).ok().flatten())
    } else {
        None
    };
    // Fetch the product before composing the prompt so we can pass
    // editorial_rules and the PR template set into compose_execution_prompt.
    let (
        product_editorial_rules,
        row_effort,
        row_model_override,
        product_default_model,
        product_dispatch_preamble,
        row_driver,
        product_default_driver,
    ) = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => {
            let product = work_db.get_product(&task.product_id).ok().flatten();
            let editorial_rules = product.as_ref().and_then(|p| p.editorial_rules.clone());
            let product_default_model = product.as_ref().and_then(|p| p.default_model.clone());
            let product_default_driver = product.as_ref().and_then(|p| p.default_driver.clone());
            let dispatch_preamble = product.and_then(|p| p.dispatch_preamble).filter(|s| !s.is_empty());
            (
                editorial_rules,
                task.effort_level,
                task.model_override.clone(),
                product_default_model,
                dispatch_preamble,
                task.driver.clone(),
                product_default_driver,
            )
        }
        _ => (None, None, None, None, None, None, None),
    };
    // Load the PR template for editorial-rules prompt injection.
    let pr_template_product_id = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task.product_id.as_str(),
        _ => "",
    };
    let pr_template_lease_id = execution.cube_lease_id.as_deref().unwrap_or("");
    let pr_template_set = crate::pr_template::load(pr_template_product_id, pr_template_lease_id, workspace_path);
    // Maint task 6: an `automation_triage` execution renders the triage
    // preamble (decision-marker contract + "do not do the work / do not
    // open a PR" guardrails) instead of the ordinary implementer prompt.
    // Its `work_item_id` is the automation id, so we read the automation
    // directly. If the automation vanished mid-flight, fall back to the
    // generic prompt so the worker at least has workspace context.
    //
    // P992 task 6: a `pr_review` execution renders the reviewer prompt
    // instead of the ordinary implementer prompt. Its `work_item_id` is
    // the producing task id, so we read the task to get the PR context.
    // If the task or its pr_url cannot be resolved, fall back to the
    // generic prompt (reviewer still gets workspace context but a weaker
    // framing — better than no spawn at all).
    let prompt_text = if execution.kind == ExecutionKind::AutomationTriage {
        match work_db.get_automation(&execution.work_item_id) {
            Ok(Some(automation)) => {
                let product_name = work_db
                    .get_product(&automation.product_id)
                    .ok()
                    .flatten()
                    .map(|p| p.name)
                    .unwrap_or_else(|| automation.product_id.clone());
                // Best-effort: a failed sibling lookup degrades the
                // preamble to its pre-dedup form rather than costing the
                // spawn. The hard gate at create time still holds.
                let siblings = work_db
                    .list_automation_sibling_tasks(&automation.id)
                    .unwrap_or_else(|err| {
                        tracing::warn!(
                            execution_id = %execution.id,
                            automation_id = %automation.id,
                            ?err,
                            "could not load already-tracked tasks for the triage preamble; \
                             rendering without the dedup section",
                        );
                        Vec::new()
                    });
                // Layer-0 context injection (automation-duplicate-work
                // investigation, 2026-07-14, §4): gather in-flight and
                // recently-merged automation work for the WHOLE product —
                // across all automations, not just this one — so the agent
                // can decline a candidate that overlaps a sibling
                // automation's recent output. Failures here degrade to an
                // empty context rather than blocking the spawn.
                let since_epoch = boss_engine_utils::epoch_time::now_epoch_secs()
                    - crate::automation_triage::RECENTLY_MERGED_WINDOW_SECS;
                let open_tasks = work_db
                    .list_open_automation_tasks_for_product(&automation.product_id)
                    .unwrap_or_default();
                let merged_tasks = work_db
                    .list_recently_completed_automation_tasks_for_product(&automation.product_id, since_epoch)
                    .unwrap_or_default();
                let triage_context = crate::automation_triage::TriageContext::from_rows(open_tasks, merged_tasks);
                crate::automation_triage::render_triage_preamble(&automation, &product_name, &siblings, &triage_context)
            }
            other => {
                tracing::warn!(
                    execution_id = %execution.id,
                    automation_id = %execution.work_item_id,
                    resolved = ?other.as_ref().map(|o| o.is_some()),
                    "automation_triage execution could not resolve its automation; \
                     falling back to generic prompt",
                );
                compose_execution_prompt(
                    ExecutionPromptParams::builder()
                        .execution(execution)
                        .work_item(work_item)
                        .workspace_path(workspace_path)
                        .maybe_parent_project(parent_project.as_ref())
                        .maybe_cube_change_id(cube_change_id)
                        .maybe_conflict_attempt(conflict_attempt.as_ref())
                        .maybe_recovery_branch(recovery_branch.as_deref())
                        .maybe_ci_attempt(ci_attempt.as_ref())
                        .maybe_editorial_rules(product_editorial_rules.as_ref())
                        .pr_template_set(&pr_template_set)
                        .editorial_enabled(editorial_enabled)
                        .worker_signal_proposals_seam_enabled(worker_signal_proposals_seam_enabled)
                        .build(),
                )
            }
        }
    } else if execution.kind == ExecutionKind::PrReview {
        let task_name = work_item_name(work_item);
        let task_description = match work_item {
            WorkItem::Task(task) | WorkItem::Chore(task) => task.description.as_str(),
            _ => "",
        };
        let pr_url = work_item_pr_url(work_item).unwrap_or_default();
        if pr_url.is_empty() {
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                "pr_review execution: producing task has no pr_url; \
                 falling back to generic prompt — review will lack PR context",
            );
            compose_execution_prompt(
                ExecutionPromptParams::builder()
                    .execution(execution)
                    .work_item(work_item)
                    .workspace_path(workspace_path)
                    .maybe_parent_project(parent_project.as_ref())
                    .maybe_cube_change_id(cube_change_id)
                    .maybe_conflict_attempt(conflict_attempt.as_ref())
                    .maybe_recovery_branch(recovery_branch.as_deref())
                    .maybe_ci_attempt(ci_attempt.as_ref())
                    .maybe_editorial_rules(product_editorial_rules.as_ref())
                    .pr_template_set(&pr_template_set)
                    .editorial_enabled(editorial_enabled)
                    .worker_signal_proposals_seam_enabled(worker_signal_proposals_seam_enabled)
                    .build(),
            )
        } else {
            // Pre-fetch PR metadata so the reviewer starts with the full diff
            // context (base/head SHAs, changed files) rather than discovering
            // it turn-by-turn. Fail open on error — the URL-only prompt is
            // still functional.
            let mut pr_review_context = fetch_pr_review_context(pr_url).await;
            // 2026-07-01 revision-review experiment: tell the reviewer what
            // head SHA the PR was already reviewed up to, so a revision-
            // triggered pass can prioritise the delta. Resolved via the
            // review-cycle root (chain root for a revision, the task itself
            // otherwise) so the value reflects the PR's actual review
            // history rather than resetting for every fresh revision task
            // row — see `WorkDb::review_cycle_root_id`.
            if let Some(ref mut ctx) = pr_review_context {
                let cycle_root_id = work_db.review_cycle_root_id(&execution.work_item_id);
                ctx.last_reviewed_sha = work_db
                    .get_task_review_cycle_state(&cycle_root_id)
                    .ok()
                    .and_then(|(_, sha)| sha);
            }
            if let Some(ref ctx) = pr_review_context {
                tracing::info!(
                    execution_id = %execution.id,
                    pr_url,
                    pr_number = ctx.pr_number,
                    head_sha = %ctx.head_sha,
                    changed_files = ctx.changed_files.len(),
                    last_reviewed_sha = ?ctx.last_reviewed_sha,
                    "pr_review execution: pre-fetched PR metadata for reviewer context",
                );
            } else {
                tracing::warn!(
                    execution_id = %execution.id,
                    pr_url,
                    "pr_review execution: PR metadata fetch failed; reviewer will use URL-only prompt",
                );
            }
            // Fetch the diff unconditionally (independent of
            // max_embed_diff_lines) so the mechanical Boss-construct sweep
            // below always runs, even when diff embedding is disabled via
            // BOSS_MAX_EMBED_DIFF_LINES=0. Embedding the fetched diff into
            // the reviewer's initial prompt is still gated on
            // max_embed_diff_lines so operators can disable that separately.
            if let Some(ref mut ctx) = pr_review_context
                && let Some(diff) = fetch_pr_diff(pr_url).await
            {
                // Mechanical assist for the agent-isms "Boss-construct
                // references" sub-rule: sweep the diff's added lines for bare
                // T<n>/P<n> tokens regardless of whether the diff ends up
                // embedded, so a large diff the reviewer fetches itself still
                // gets forced-disposition candidates.
                let diff_hits = crate::boss_construct_scan::scan_diff_added_lines(&diff);
                ctx.boss_construct_refs
                    .extend(crate::boss_construct_scan::hit_lines(&diff_hits));

                let line_count = diff.lines().count() as u64;
                if max_embed_diff_lines > 0 && line_count <= max_embed_diff_lines {
                    tracing::info!(
                        execution_id = %execution.id,
                        pr_url,
                        line_count,
                        max_embed_diff_lines,
                        "pr_review execution: embedding diff in reviewer prompt",
                    );
                    ctx.diff_content = Some(diff);
                } else {
                    tracing::debug!(
                        execution_id = %execution.id,
                        pr_url,
                        line_count,
                        max_embed_diff_lines,
                        "pr_review execution: diff too large to embed, or \
                         embedding disabled; reviewer will fetch it",
                    );
                }
            }
            // Use the changed-file list (when available) to classify the review
            // scope accurately, instead of always defaulting to Code.
            let scope = match &pr_review_context {
                Some(ctx) => {
                    let files: Vec<&str> = ctx.changed_files.iter().map(String::as_str).collect();
                    crate::pr_review::classify_changed_files(&files)
                }
                None => crate::pr_review::ReviewScope::Code,
            };
            let reviewer_repo_slug = crate::completion::parse_repo_slug(&execution.repo_remote_url)
                .unwrap_or_else(|_| "<owner/repo>".to_owned());
            crate::pr_review::render_reviewer_initial_prompt(
                task_name,
                task_description,
                pr_url,
                &crate::structured_output::default_path_string(&execution.id),
                scope,
                pr_review_context.as_ref(),
                &reviewer_repo_slug,
            )
        }
    } else if execution.kind == ExecutionKind::AnswerAgent {
        // P3b: an `answer_agent` execution renders the answer-agent prompt
        // (doc content, comment, thread history, reply instructions) instead
        // of the ordinary implementer prompt. Its `work_item_id` is the
        // comment id (see `WorkDb::create_answer_agent_execution`).
        compose_answer_agent_prompt(work_db, execution).await
    } else {
        compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(execution)
                .work_item(work_item)
                .workspace_path(workspace_path)
                .maybe_parent_project(parent_project.as_ref())
                .maybe_cube_change_id(cube_change_id)
                .maybe_conflict_attempt(conflict_attempt.as_ref())
                .maybe_recovery_branch(recovery_branch.as_deref())
                .maybe_ci_attempt(ci_attempt.as_ref())
                .maybe_editorial_rules(product_editorial_rules.as_ref())
                .pr_template_set(&pr_template_set)
                .editorial_enabled(editorial_enabled)
                .worker_signal_proposals_seam_enabled(worker_signal_proposals_seam_enabled)
                .merge_order_preservation(&merge_order_preservation)
                .build(),
        )
    };
    // Products and projects do not have a TaskKind; only Task/Chore rows
    // carry one. Threaded into `resolve_spawn_config` so design-family kinds
    // (`Design` / `DesignPostmortem` / `Investigation`) floor to Opus
    // regardless of effort level, and reused below for the capability gate.
    let work_item_kind = work_item_task_kind_enum(work_item);
    let spawn_config = resolve_spawn_config(
        row_effort,
        row_model_override.as_deref(),
        pool_model_override_for_worker_id(worker_id),
        product_default_model.as_deref(),
        row_driver.as_deref(),
        product_default_driver.as_deref(),
        work_item_kind,
    );

    // Capability gate: fail closed before the pane spawns when the resolved
    // driver cannot satisfy the work-item kind's requirements. Products and
    // projects do not have a TaskKind; only Task/Chore rows are gated.
    if let Some(kind) = work_item_kind {
        let registry = crate::driver::DriverRegistry::default();
        match registry.resolver(&spawn_config.driver) {
            Some(resolver) => {
                resolver
                    .check_dispatch(kind)
                    .map_err(|e| anyhow::anyhow!("capability gate: {e}"))?;
            }
            None => {
                anyhow::bail!(
                    "capability gate: driver '{}' is not registered; \
                     cannot dispatch {} work item",
                    spawn_config.driver,
                    kind,
                );
            }
        }
    }

    // Per-level prompt addendum lands at the very top of the file
    // (design §Q2: "concatenated to .claude/initial-prompt.txt
    // BEFORE the existing prompt body"). The existing task /
    // design / conflict-resolution framing must stay byte-identical
    // when the addendum is `None`.
    let prompt_text = match spawn_config.prompt_addendum {
        Some(addendum) => format!("{}\n\n{}", addendum, prompt_text),
        None => prompt_text,
    };

    // Product dispatch preamble is prepended before the effort
    // addendum, with visible bracket markers so humans reading
    // transcripts know what was injected by the engine.
    // Empty / null preamble → today's behaviour, no change.
    let prompt_text = match product_dispatch_preamble {
        Some(preamble) => {
            format!(
                "[product-preamble]\n{}\n[/product-preamble]\n\n{}",
                preamble, prompt_text
            )
        }
        None => prompt_text,
    };

    Ok(ComposedWorkerSpawn {
        prompt_text,
        spawn_config,
    })
}

#[cfg(test)]
mod compose_worker_spawn_tests {
    //! Targeted tests for `compose_worker_spawn` covering the `pr_review`
    //! branch: branch selection (PrReview vs. other kinds), the no-pr-url
    //! fallback to the generic implementer prompt, and the URL-only reviewer
    //! prompt rendered when the PR metadata fetch fails.
    use super::*;
    use crate::work::Task;
    use boss_protocol::{ExecutionKind, ExecutionStatus, TaskKind, TaskStatus};
    use tempfile::TempDir;

    fn pr_review_execution() -> WorkExecution {
        WorkExecution::builder()
            .id("exec_rev123_01")
            .work_item_id("task-pr-1")
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    fn chore_execution() -> WorkExecution {
        WorkExecution::builder()
            .id("exec_chore123_01")
            .work_item_id("task-chore-1")
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    fn task_without_pr(task_id: &str) -> WorkItem {
        WorkItem::Chore(
            Task::builder()
                .id(task_id)
                .product_id("prod-1")
                .kind(TaskKind::Chore)
                .name("Add a new feature")
                .description("Feature description.")
                .status(TaskStatus::Todo)
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .autostart(false)
                .build(),
        )
    }

    fn task_with_pr(task_id: &str, pr_url: &str) -> WorkItem {
        match task_without_pr(task_id) {
            WorkItem::Chore(mut task) => {
                task.pr_url = Some(pr_url.into());
                WorkItem::Chore(task)
            }
            other => other,
        }
    }

    fn open_memory_db() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    /// When a `pr_review` execution's producing task has no `pr_url`, the
    /// branch falls back to the generic implementer prompt rather than
    /// rendering a reviewer prompt with no target PR.
    #[tokio::test]
    async fn pr_review_no_pr_url_falls_back_to_generic_prompt() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let work_item = task_without_pr("task-pr-1");

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            (false, 0, false),
        )
        .await
        .unwrap();

        assert!(
            !composed.prompt_text.contains("# PR review"),
            "pr_review with no pr_url must not render the reviewer prompt:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("exec_rev123_01"),
            "fallback generic prompt must contain the execution id:\n{}",
            composed.prompt_text,
        );
    }

    /// When a `pr_review` execution has a `pr_url`, `compose_worker_spawn`
    /// calls `render_reviewer_initial_prompt` even when the upstream
    /// `fetch_pr_review_context` fails (no real `gh` in tests) — the
    /// URL-only reviewer prompt is still correctly formatted.
    #[tokio::test]
    async fn pr_review_with_pr_url_renders_reviewer_prompt() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let pr_url = "https://github.com/org/repo/pull/42";
        let work_item = task_with_pr("task-pr-1", pr_url);

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            (false, 0, false),
        )
        .await
        .unwrap();

        assert!(
            composed.prompt_text.contains("# PR review"),
            "pr_review with pr_url must render the reviewer prompt header:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("independent PR reviewer"),
            "reviewer prompt must identify the agent role:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains(pr_url),
            "reviewer prompt must include the PR URL:\n{}",
            composed.prompt_text,
        );
    }

    /// A non-`pr_review` execution kind (e.g. `ChoreImplementation`) must not
    /// enter the `pr_review` branch at all and must produce the generic
    /// implementer prompt.
    #[tokio::test]
    async fn non_pr_review_execution_routes_to_generic_prompt() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = chore_execution();
        let work_item = task_without_pr("task-chore-1");

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            (false, 0, false),
        )
        .await
        .unwrap();

        assert!(
            !composed.prompt_text.contains("# PR review"),
            "non-pr_review execution must not render the reviewer prompt:\n{}",
            composed.prompt_text,
        );
        assert!(
            !composed.prompt_text.contains("independent PR reviewer"),
            "non-pr_review execution must not contain reviewer role text:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("exec_chore123_01"),
            "generic prompt must contain the execution id:\n{}",
            composed.prompt_text,
        );
    }

    /// The reviewer prompt must not include implementer-only directives like
    /// "expected branch name" — reviewers must not commit or push anything.
    #[tokio::test]
    async fn pr_review_prompt_omits_branch_push_directives() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let pr_url = "https://github.com/org/repo/pull/99";
        let work_item = task_with_pr("task-pr-1", pr_url);

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            (false, 0, false),
        )
        .await
        .unwrap();

        assert!(
            !composed.prompt_text.contains("expected branch name"),
            "reviewer prompt must not include the expected branch name directive:\n{}",
            composed.prompt_text,
        );
    }
}
