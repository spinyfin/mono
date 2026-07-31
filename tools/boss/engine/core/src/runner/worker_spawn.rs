//! Worker-spawn composition: [`compose_worker_spawn`] assembles the worker
//! prompt + resolved effort/model config ([`ComposedWorkerSpawn`]), fetching
//! PR review context and diffs for the `pr_review` path.

use std::path::Path;

use boss_engine_gh_invocation::gh_output;
use boss_gh_telemetry::{callers, scope as gh_scope};

use crate::coordinator::{PoolDispatchPolicy, pool_dispatch_policy_for_worker_id};
use crate::effort::{DriverResolutionSource, SpawnConfig, SpawnResolutionInput, resolve_spawn_config_in};
use crate::structured_output::StructuredOutputKind;
use crate::work::{REASON_ALLOCATION, REASON_LEGACY_PERCENTAGE, WorkDb, WorkExecution, WorkItem};
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

/// Editorial + worker-proposal-seam knobs [`compose_worker_spawn`] threads
/// into prompt composition, bundled into one named struct (rather than
/// positional bools) so call sites state what they are setting. Each
/// `*_proposals_seam_enabled` field mirrors a feature flag of the same name
/// gating one seam of the worker-proposal-API migration; see
/// [`super::prompt::ExecutionPromptParams`]'s matching fields for what each
/// one does to the rendered prompt. All fields default OFF, matching every
/// flag's registry default.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct WorkerSpawnOpts {
    pub(crate) editorial_enabled: bool,
    pub(crate) max_embed_diff_lines: u64,
    pub(crate) worker_signal_proposals_seam_enabled: bool,
    pub(crate) deferred_scope_proposals_seam_enabled: bool,
    pub(crate) followup_proposals_seam_enabled: bool,
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

    // incident-002: deterministically scan the worker's *narrative*
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
        // deletion tripwire for conflict-resolution reviews — incident-002).
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
    let output = gh_scope(callers::WORKER_SPAWN, gh_output(&["pr", "diff", pr_url]))
        .await
        .ok()?;

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

/// The task-driver value fed into `resolve_spawn_config`'s driver-resolution
/// step for this spawn. Review/automation-pool workers dispatch on
/// `pool_policy`'s fixed driver unconditionally — ignoring `row_driver`
/// entirely, so the driver of the row under review/automation never leaks
/// into who reviews/automates it (see
/// [`crate::coordinator::pool_dispatch_policy_for_worker_id`]'s doc comment).
/// Main-pool workers (`pool_policy = None`) fall through to `row_driver` (or
/// `None`, letting `resolve_spawn_config` fall through to the
/// product/engine default) exactly as before.
///
/// A separate, standalone function for the same reason as
/// [`check_model_driver_compatibility`]: testable without the full async
/// prompt-composition machinery.
fn effective_task_driver_for_worker(pool_policy: Option<PoolDispatchPolicy>, row_driver: Option<&str>) -> Option<&str> {
    match pool_policy {
        Some(policy) => Some(policy.driver),
        None => row_driver,
    }
}

fn resolved_driver_source(
    pool_policy: Option<PoolDispatchPolicy>,
    row_driver: Option<&str>,
    product_default_driver: Option<&str>,
    has_allocation: bool,
) -> DriverResolutionSource {
    if pool_policy.is_some() {
        DriverResolutionSource::PoolPolicy
    } else if row_driver.map(str::trim).is_some_and(|value| !value.is_empty()) {
        DriverResolutionSource::TaskOverride
    } else if product_default_driver
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        DriverResolutionSource::ProductDefault
    } else if has_allocation {
        DriverResolutionSource::TrafficAllocation
    } else {
        DriverResolutionSource::EngineDefault
    }
}

/// Model/driver compatibility gate. Returns an error naming both the driver
/// and the model when `model` is not one `driver_descriptor`'s
/// [`crate::driver::ModelMenu::model_belongs_to_driver`] recognises as
/// belonging to that driver.
///
/// A separate, standalone function (rather than inlined at the
/// [`compose_worker_spawn`] call site) so it can be exercised directly by
/// unit tests without standing up the full async prompt-composition
/// machinery — see the `model_driver_gate_tests` module below.
fn check_model_driver_compatibility(
    driver_descriptor: &crate::driver::DriverDescriptor,
    model: &str,
) -> anyhow::Result<()> {
    if (driver_descriptor.model_menu.model_belongs_to_driver)(model) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "model/driver gate: resolved model {model:?} is not valid for driver {:?} ({}); \
             refusing to dispatch a mismatched pair instead of letting the CLI reject it after spawn",
            driver_descriptor.name,
            driver_descriptor.label,
        ))
    }
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
    // Bundled (rather than five positional bools) to keep the parameter
    // count under clippy::too_many_arguments AND so call sites name what
    // they set instead of relying on positional order — a transposed pair
    // of seam flags here would compile silently and mis-gate a prompt.
    editorial_opts: WorkerSpawnOpts,
) -> anyhow::Result<ComposedWorkerSpawn> {
    let WorkerSpawnOpts {
        editorial_enabled,
        max_embed_diff_lines,
        worker_signal_proposals_seam_enabled,
        deferred_scope_proposals_seam_enabled,
        followup_proposals_seam_enabled,
    } = editorial_opts;
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
    // editorial_rules, dispatch_preamble, and design_guidance into
    // compose_execution_prompt. Derived via `WorkItem::product_id()` — which
    // covers every variant, not just Task/Chore — so a `ProductDesign`
    // execution (whose work item IS the product) resolves its own product
    // row instead of silently losing it to a `_ => None` fallthrough. A DB
    // error reading the row propagates via `?` rather than being swallowed
    // into "no product": a genuine read failure must be loud, never
    // rendered as an empty preamble/guidance block.
    let product = work_db.get_product(work_item.product_id())?;
    // `default_model` / `default_driver` / `editorial_rules` stay scoped to
    // Task/Chore executions only — the same scope they had before this
    // widened lookup. Those three drive dispatch decisions (which model,
    // which driver, which PR-surface rules) for an ordinary work-item
    // execution, and widening them to Product-/Project-scoped executions
    // (e.g. `ProductDesign`) would silently change what model/driver a
    // product-level run dispatches to — a behavior change this chore does
    // not intend. Only `dispatch_preamble` and `design_guidance` are meant
    // to reach every execution kind on the product.
    let is_task_or_chore = matches!(work_item, WorkItem::Task(_) | WorkItem::Chore(_));
    let product_editorial_rules = if is_task_or_chore {
        product.as_ref().and_then(|p| p.editorial_rules.clone())
    } else {
        None
    };
    let product_default_model = if is_task_or_chore {
        product.as_ref().and_then(|p| p.default_model.clone())
    } else {
        None
    };
    let product_default_driver = if is_task_or_chore {
        product.as_ref().and_then(|p| p.default_driver.clone())
    } else {
        None
    };
    let allocated_driver = if is_task_or_chore {
        work_db
            .get_execution_driver_decision(&execution.id)
            .ok()
            .flatten()
            .filter(|decision| matches!(decision.reason, REASON_ALLOCATION | REASON_LEGACY_PERCENTAGE))
            .and_then(|decision| decision.driver)
    } else {
        None
    };
    let product_dispatch_preamble = product
        .as_ref()
        .and_then(|p| p.dispatch_preamble.clone())
        .filter(|s| !s.is_empty());
    let product_design_guidance = product
        .as_ref()
        .and_then(|p| p.design_guidance.clone())
        .filter(|s| !s.is_empty());
    let (row_effort, row_model_override, row_driver, row_reasoning) = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => (
            task.effort_level,
            task.model_override.clone(),
            task.driver.clone(),
            task.reasoning,
        ),
        _ => (None, None, None, None),
    };
    // Load the PR template for editorial-rules prompt injection. Uses
    // `WorkItem::product_id()` (total over every variant) rather than a
    // Task/Chore-only match, so a Product-/Project-scoped execution (e.g.
    // `ProductDesign`) resolves its product's PR template set instead of
    // always loading an empty one.
    let pr_template_product_id = work_item.product_id();
    let pr_template_lease_id = execution.cube_lease_id.as_deref().unwrap_or("");
    let pr_template_set = crate::pr_template::load(pr_template_product_id, pr_template_lease_id, workspace_path);
    // Maint task 6: an `automation_triage` execution renders the triage
    // preamble (decision-marker contract + "do not do the work / do not
    // open a PR" guardrails) instead of the ordinary implementer prompt.
    // Its `work_item_id` is the automation id, so we read the automation
    // directly. If the automation vanished mid-flight, fall back to the
    // generic prompt so the worker at least has workspace context.
    //
    // Likewise, a `pr_review` execution renders the reviewer prompt
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
                crate::automation_triage::render_triage_preamble(
                    &automation,
                    &product_name,
                    &siblings,
                    &triage_context,
                    &crate::structured_output::default_path_string(&execution.id, StructuredOutputKind::TriageDecision),
                )
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
                        .maybe_ci_attempt(ci_attempt.as_ref())
                        .maybe_editorial_rules(product_editorial_rules.as_ref())
                        .maybe_design_guidance(product_design_guidance.as_deref())
                        .pr_template_set(&pr_template_set)
                        .editorial_enabled(editorial_enabled)
                        .worker_signal_proposals_seam_enabled(worker_signal_proposals_seam_enabled)
                        .deferred_scope_proposals_seam_enabled(deferred_scope_proposals_seam_enabled)
                        .followup_proposals_seam_enabled(followup_proposals_seam_enabled)
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
                    .maybe_ci_attempt(ci_attempt.as_ref())
                    .maybe_editorial_rules(product_editorial_rules.as_ref())
                    .maybe_design_guidance(product_design_guidance.as_deref())
                    .pr_template_set(&pr_template_set)
                    .editorial_enabled(editorial_enabled)
                    .worker_signal_proposals_seam_enabled(worker_signal_proposals_seam_enabled)
                    .deferred_scope_proposals_seam_enabled(deferred_scope_proposals_seam_enabled)
                    .followup_proposals_seam_enabled(followup_proposals_seam_enabled)
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
                &crate::structured_output::default_path_string(&execution.id, StructuredOutputKind::ReviewResult),
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
                .maybe_ci_attempt(ci_attempt.as_ref())
                .maybe_editorial_rules(product_editorial_rules.as_ref())
                .maybe_design_guidance(product_design_guidance.as_deref())
                .pr_template_set(&pr_template_set)
                .editorial_enabled(editorial_enabled)
                .worker_signal_proposals_seam_enabled(worker_signal_proposals_seam_enabled)
                .deferred_scope_proposals_seam_enabled(deferred_scope_proposals_seam_enabled)
                .followup_proposals_seam_enabled(followup_proposals_seam_enabled)
                .merge_order_preservation(&merge_order_preservation)
                .build(),
        )
    };
    // Products and projects do not have a TaskKind; only Task/Chore rows
    // carry one. Threaded into `resolve_spawn_config` so design-family kinds
    // (`Design` / `DesignPostmortem` / `Investigation`) floor to the selected
    // driver's investigation tier *on rows that predate the `reasoning`
    // column* — newer rows reach the same place through `row_reasoning`,
    // which the resolver consults first. Reused below for the capability gate.
    let work_item_kind = work_item_task_kind_enum(work_item);
    let registry = crate::driver::DriverRegistry::default();
    // Single resolution point for review/automation dispatch policy — see
    // `pool_dispatch_policy_for_worker_id`'s doc comment. `None` for
    // main-pool workers, which dispatch on the row's own `driver` column.
    let pool_policy = pool_dispatch_policy_for_worker_id(worker_id);
    // The allocator decision occupies the final driver fallback position,
    // behind both live pin levels. Keeping it separate until here lets the
    // resolver log honest provenance instead of calling an allocation a
    // product default.
    let fallback_driver = product_default_driver.as_deref().or(allocated_driver.as_deref());
    let driver_source = resolved_driver_source(
        pool_policy,
        row_driver.as_deref(),
        product_default_driver.as_deref(),
        allocated_driver.is_some(),
    );
    let spawn_input = SpawnResolutionInput::builder()
        .maybe_effort_level(row_effort)
        .maybe_model_override(row_model_override.as_deref())
        .maybe_pool_model_override(pool_policy.map(|p| p.model_tier))
        .maybe_product_default_model(product_default_model.as_deref())
        .maybe_task_driver(effective_task_driver_for_worker(pool_policy, row_driver.as_deref()))
        .maybe_product_default_driver(fallback_driver)
        .driver_source(driver_source)
        .maybe_kind(work_item_kind)
        .maybe_reasoning(row_reasoning)
        .build();
    let spawn_config = resolve_spawn_config_in(&registry, &spawn_input)
        .map_err(|e| anyhow::anyhow!("effort/model resolution: {e}"))?;

    tracing::info!(
        execution_id = %execution.id,
        driver = %spawn_config.driver,
        model = %spawn_config.model,
        driver_source = spawn_config.driver_source.as_str(),
        model_source = spawn_config.model_source.as_str(),
        "resolved worker model/driver pair",
    );

    // Model/driver compatibility gate: fail closed before the pane spawns
    // when the resolved model does not belong to the resolved driver's
    // vocabulary. `resolve_spawn_config_in` now guarantees this invariant;
    // the independent gate deliberately remains as defence in depth so a
    // future resolver regression or hand-built config still fails closed
    // before any CLI is launched.
    //
    // `spawn_config.driver` was already validated against this same
    // `registry` by `resolve_spawn_config_in` above (it returns
    // `UnknownDriverError` and bails before reaching here on an
    // unregistered slug), so the lookup cannot fail.
    let resolved_driver = registry
        .get(&spawn_config.driver)
        .expect("slug validated by resolve_spawn_config_in");
    check_model_driver_compatibility(resolved_driver.descriptor(), &spawn_config.model)?;

    // Capability gate: fail closed before the pane spawns when the resolved
    // driver cannot satisfy the work-item kind's requirements. Products and
    // projects do not have a TaskKind; only Task/Chore rows are gated.
    if let Some(kind) = work_item_kind {
        let resolver = registry
            .resolver(&spawn_config.driver)
            .expect("slug validated by resolve_spawn_config_in");
        resolver
            .check_dispatch(kind, Some(&execution.kind))
            .map_err(|e| anyhow::anyhow!("capability gate: {e}"))?;
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
mod reviewer_pool_policy_tests {
    //! Verifies review/automation dispatch policy is decoupled from the
    //! reviewed/automated row's own `driver` column — the single resolution
    //! point is `crate::coordinator::pool_dispatch_policy_for_worker_id`,
    //! and [`effective_task_driver_for_worker`] is what applies it ahead of
    //! `resolve_spawn_config`.
    use super::*;

    #[test]
    fn reviewer_effective_driver_ignores_the_reviewed_rows_driver_column() {
        let policy =
            crate::coordinator::pool_dispatch_policy_for_worker_id("review-1").expect("review worker has a policy");
        for row_driver in [Some("codex"), Some("grok"), Some("claude"), None] {
            assert_eq!(
                effective_task_driver_for_worker(Some(policy), row_driver),
                Some("claude"),
                "reviewer driver must not vary with the reviewed row's driver column, got row_driver={row_driver:?}"
            );
        }
    }

    #[test]
    fn main_pool_effective_driver_still_falls_through_to_row_driver() {
        assert_eq!(effective_task_driver_for_worker(None, Some("codex")), Some("codex"));
        assert_eq!(effective_task_driver_for_worker(None, None), None);
    }

    #[test]
    fn driver_provenance_follows_the_same_precedence_as_driver_resolution() {
        let pool = crate::coordinator::pool_dispatch_policy_for_worker_id("review-1");
        assert_eq!(
            resolved_driver_source(pool, Some("codex"), Some("grok"), true),
            DriverResolutionSource::PoolPolicy,
        );
        assert_eq!(
            resolved_driver_source(None, Some("codex"), Some("grok"), true),
            DriverResolutionSource::TaskOverride,
        );
        assert_eq!(
            resolved_driver_source(None, None, Some("grok"), true),
            DriverResolutionSource::ProductDefault,
        );
        assert_eq!(
            resolved_driver_source(None, None, None, true),
            DriverResolutionSource::TrafficAllocation,
        );
        assert_eq!(
            resolved_driver_source(None, None, None, false),
            DriverResolutionSource::EngineDefault,
        );
    }

    /// Resolves a review-pool spawn end to end (policy → effective driver →
    /// `resolve_spawn_config`) for a row whose own `driver` column is
    /// `row_driver`, mirroring exactly what `compose_worker_spawn` does.
    fn resolve_reviewer_spawn(row_driver: Option<&str>) -> crate::effort::SpawnConfig {
        let pool_policy = crate::coordinator::pool_dispatch_policy_for_worker_id("review-1").unwrap();
        let registry = crate::driver::DriverRegistry::default();
        let input = crate::effort::SpawnResolutionInput::builder()
            .maybe_task_driver(effective_task_driver_for_worker(Some(pool_policy), row_driver))
            .maybe_pool_model_override(Some(pool_policy.model_tier))
            .driver_source(DriverResolutionSource::PoolPolicy)
            .build();
        crate::effort::resolve_spawn_config_in(&registry, &input).unwrap()
    }

    #[test]
    fn codex_authored_row_gets_a_claude_reviewer_on_opus_not_a_codex_reviewer() {
        let cfg = resolve_reviewer_spawn(Some("codex"));
        assert_eq!(
            cfg.driver, "claude",
            "reviewer must dispatch on Claude, not the authored row's codex driver"
        );
        assert_eq!(cfg.model, "opus");
    }

    #[test]
    fn claude_authored_row_under_review_is_unchanged_claude_reviewer_on_opus() {
        let cfg = resolve_reviewer_spawn(Some("claude"));
        assert_eq!(cfg.driver, "claude");
        assert_eq!(cfg.model, "opus");
    }

    #[test]
    fn untagged_row_under_review_still_gets_a_claude_reviewer_on_opus() {
        let cfg = resolve_reviewer_spawn(None);
        assert_eq!(cfg.driver, "claude");
        assert_eq!(cfg.model, "opus");
    }
}

#[cfg(test)]
mod model_driver_gate_tests {
    //! Targeted tests for [`check_model_driver_compatibility`] — the
    //! spawn-time gate that fails closed when the resolved model does not
    //! belong to the resolved driver's `ModelMenu`. Exercises the same
    //! `DriverRegistry::default()` the production spawn path builds, so a
    //! driver's real vocabulary (not a stub) is what gets checked.
    use super::*;

    #[test]
    fn rejects_a_claude_alias_dispatched_on_the_codex_driver() {
        // The exact bug this gate exists to catch: the review/automation
        // pool override used to hand "opus" straight to whatever driver the
        // row resolved to, so a codex-driver reviewer 400'd before emitting
        // a token. The gate must refuse to dispatch that pair.
        let registry = crate::driver::DriverRegistry::default();
        let codex = registry.get("codex").expect("codex is registered");
        let err = check_model_driver_compatibility(codex.descriptor(), "opus").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("opus"), "error must name the model, got: {msg:?}");
        assert!(msg.contains("codex"), "error must name the driver, got: {msg:?}");
    }

    #[test]
    fn rejects_a_codex_model_dispatched_on_the_claude_driver() {
        let registry = crate::driver::DriverRegistry::default();
        let claude = registry.get("claude").expect("claude is registered");
        let err = check_model_driver_compatibility(claude.descriptor(), "gpt-5.6-sol").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gpt-5.6-sol"));
        assert!(msg.contains("claude"));
    }

    #[test]
    fn accepts_every_registered_drivers_own_engine_default() {
        let registry = crate::driver::DriverRegistry::default();
        for slug in registry.slugs() {
            let driver = registry.get(slug).expect("slug came from registry.slugs()");
            let default_model = driver.descriptor().model_menu.engine_default;
            check_model_driver_compatibility(driver.descriptor(), default_model).unwrap_or_else(|e| {
                panic!("driver {slug:?}'s own engine_default {default_model:?} must pass its own gate: {e}")
            });
        }
    }

    #[test]
    fn accepts_the_reviewer_pools_strong_tier_for_every_registered_driver() {
        // Mirrors what `compose_worker_spawn` actually resolves for a
        // review/automation-pool row: `PoolModelTier::Strong` through each
        // driver's own `model_for_reasoning(Investigation)`.
        let registry = crate::driver::DriverRegistry::default();
        for slug in registry.slugs() {
            let driver = registry.get(slug).expect("slug came from registry.slugs()");
            let strong_model =
                (driver.descriptor().model_menu.model_for_reasoning)(boss_protocol::ReasoningMode::Investigation);
            check_model_driver_compatibility(driver.descriptor(), strong_model).unwrap_or_else(|e| {
                panic!("driver {slug:?}'s own strong-tier model {strong_model:?} must pass its own gate: {e}")
            });
        }
    }
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
            WorkerSpawnOpts::default(),
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
            WorkerSpawnOpts::default(),
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
            WorkerSpawnOpts::default(),
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
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();

        assert!(
            !composed.prompt_text.contains("expected branch name"),
            "reviewer prompt must not include the expected branch name directive:\n{}",
            composed.prompt_text,
        );
    }

    /// A `Product`-scoped execution (e.g. `product_design`) picks up the
    /// product's `dispatch_preamble` (widened to every execution kind), but
    /// must NOT pick up `default_model` / `default_driver` /
    /// `editorial_rules` — those stay scoped to Task/Chore executions only,
    /// exactly as they were before `WorkItem::product_id()` replaced the
    /// Task/Chore-only match in product derivation. A product-level run
    /// dispatching on a different model/driver than before this widening
    /// would be a silent, unstated behavior change.
    #[tokio::test]
    async fn product_scoped_execution_gets_preamble_but_not_default_model_or_driver() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let product = db
            .create_product(
                crate::work::CreateProductInput::builder()
                    .name("Widget Co")
                    .repo_remote_url("git@github.com:org/widget.git")
                    .build(),
            )
            .unwrap();
        let patch = crate::work::WorkItemPatch::builder()
            .default_model("sonnet")
            .default_driver("codex")
            .dispatch_preamble("house style: terse commit messages")
            .build();
        db.update_product(&product.id, patch).unwrap();
        let product = db.get_product(&product.id).unwrap().expect("product exists");
        assert_eq!(product.default_model.as_deref(), Some("sonnet"));

        let execution = WorkExecution::builder()
            .id("exec_prod123_01")
            .work_item_id(product.id.clone())
            .kind(ExecutionKind::ProductDesign)
            .status(boss_protocol::ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/widget.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build();
        let work_item = WorkItem::Product(product);

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            WorkerSpawnOpts::default(),
        )
        .await
        .unwrap();

        assert!(
            composed.prompt_text.contains("house style: terse commit messages"),
            "product-scoped execution must still receive dispatch_preamble:\n{}",
            composed.prompt_text,
        );
        assert_ne!(
            composed.spawn_config.model, "sonnet",
            "product's default_model must not apply to a Product-scoped execution"
        );
        assert_ne!(
            composed.spawn_config.driver, "codex",
            "product's default_driver must not apply to a Product-scoped execution"
        );
    }
}
