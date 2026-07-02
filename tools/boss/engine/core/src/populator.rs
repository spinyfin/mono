//! The **Populator** — the auto-populate orchestration triggered when a
//! project's `kind = 'design'` PR merges.
//!
//! See `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`
//! (project P783) §1 "Trigger & idempotency" and the architecture overview.
//! This module is task 7 of that design: the orchestrator that wires the
//! sibling components — the idempotency ledger ([`crate::work::planner_runs`]),
//! the live doc fetch ([`crate::doc_fetcher`]), the Planner
//! ([`crate::planner`]), the validation layer ([`crate::planner_validation`]),
//! and the Materializer ([`crate::materializer`]) — into one fail-safe pass:
//!
//! ```text
//!   claim → pre-seeded check → fetch → plan → validate → apply → audit → surface
//! ```
//!
//! ## The Populator is *a* caller of the Planner, not the Planner
//!
//! It performs no inference itself; it owns steps 1, 2, 4, 5, 6, 7 of the
//! design (idempotency, fetch, validate, apply, audit, surface) and delegates
//! step 3 (infer) to the Planner. The two side-effecting network steps (doc
//! fetch + LLM plan) are injected behind [`PopulatorSteps`] so the whole
//! orchestration is unit-testable with an in-memory DB and no network.
//!
//! ## The cardinal rule: one state-mutating step
//!
//! The only step that writes task rows is the single Materializer
//! transaction. Every failure mode before that commit leaves the project
//! exactly as it was (design task `done`, pointer set, zero tasks created)
//! and records a terminal `planner_runs` outcome plus an attention item so
//! the operator — who cannot watch the run — learns what happened.
//!
//! ## Idempotency
//!
//! The first action is to *claim* the project by inserting a `planner_runs`
//! row (`outcome = 'running'`) whose UNIQUE-per-project partial index makes
//! concurrent triggers, poller restarts, and manual retries all safe: exactly
//! one populate per project. A claim conflict → clean skip.
//!
//! ## Production wiring
//!
//! The merge poller's `mark_merged` calls [`enqueue_from_merge`], which spawns
//! the pass on a background task (the poller must not block on a multi-second
//! LLM call). The api-key/network capability is *installed once at engine
//! startup* via [`install`]; contexts that never install it (the merge-poller
//! unit tests, non-server callers) no-op the spawn, so no test reaches the
//! network. This mirrors the process-wide `OnceLock` client pattern already
//! used by [`crate::planner`] and [`crate::live_status`].

use std::sync::OnceLock;

use async_trait::async_trait;

use boss_protocol::{
    CreateAttentionItemInput, DocRef, PLANNER_OUTCOME_DOC_MISSING, PLANNER_OUTCOME_FETCH_FAILED,
    PLANNER_OUTCOME_NO_BREAKDOWN, PLANNER_OUTCOME_PLANNER_FAILED, PLANNER_OUTCOME_REJECTED_CYCLE,
    PLANNER_OUTCOME_REJECTED_TOO_MANY, PLANNER_OUTCOME_SKIPPED_PRE_SEEDED, PLANNER_OUTCOME_STAGED, PlannerInput,
    PlannerOutput, ProductContext, ProjectContext, TaskBrief, TaskKind,
};

use crate::doc_fetcher::{DocFetchOutcome, fetch_design_doc};
use crate::materializer::Materializer;
use crate::planner::{PLANNER_MODEL, Planner, PlannerOutcome};
use crate::planner_validation::{ValidationResult, validate};
use crate::work::{ClaimPlannerRunInput, PlannerRunPatch, WorkDb};

/// Default hard cap on how many tasks one populate may create (design
/// §"Bounding & guardrails"). A proposal exceeding it is rejected whole —
/// never truncated. A single constant, tunable without a schema change.
pub const DEFAULT_MAX_TASKS: usize = 30;

/// `caller` value stamped on `planner_runs` rows this module claims.
/// Distinguishes trigger-initiated populates from operator / replan callers
/// in the audit ledger.
pub const CALLER_MERGE_TRIGGER: &str = "merge_trigger";

/// `kind` of the `WorkAttentionItem` the Populator raises against the design
/// task. A single kind keeps the surface simple; the outcome-specific text
/// lives in the title/body.
const ATTENTION_KIND: &str = "auto_populate";

/// Fallback ref when the project's `design_doc_branch` pointer is unset. The
/// design doc has merged, so it lives on the default branch.
const DEFAULT_DOC_REF: &str = "main";

// ---------------------------------------------------------------------------
// Trigger context
// ---------------------------------------------------------------------------

/// Everything the merge trigger knows when a design PR merges — captured at
/// the `mark_merged` call site and carried into the (possibly background)
/// pass. The design doc pointer itself is *not* carried here; it is read
/// fresh from the project inside [`Populator::run`] (the merge trigger has
/// already written it via `design_detector::on_design_pr_merged`), so the
/// Populator always fetches from the authoritative merged pointer.
#[derive(Debug, Clone)]
pub struct PopulateContext {
    pub project_id: String,
    pub product_id: String,
    /// The `kind = 'design'` task whose PR merged. Recorded on the
    /// `planner_runs` row and used as the attention item's target.
    pub design_task_id: String,
    /// For logging / provenance only.
    pub pr_url: String,
}

// ---------------------------------------------------------------------------
// Injected side effects (fetch + plan)
// ---------------------------------------------------------------------------

/// The two network-touching steps of a populate, injected so the
/// orchestration is testable without GitHub or Anthropic. Production uses
/// [`LivePopulatorSteps`]; tests use a fake returning canned outcomes.
#[async_trait]
pub trait PopulatorSteps: Send + Sync {
    /// Fetch the design doc live from GitHub at the merged ref.
    async fn fetch_doc(&self, repo_remote_url: &str, doc_path: &str, git_ref: &str) -> DocFetchOutcome;

    /// Run the Planner (LLM inference) over the assembled input.
    async fn plan(&self, input: &PlannerInput) -> PlannerOutcome;
}

/// Production [`PopulatorSteps`]: real `gh api` fetch + real Anthropic call.
pub struct LivePopulatorSteps {
    /// Anthropic API key, captured from config at engine startup. `None`
    /// degrades the plan step to [`PlannerOutcome::NoApiKey`] with no network
    /// call, exactly as `live_status` degrades.
    pub api_key: Option<String>,
}

#[async_trait]
impl PopulatorSteps for LivePopulatorSteps {
    async fn fetch_doc(&self, repo_remote_url: &str, doc_path: &str, git_ref: &str) -> DocFetchOutcome {
        fetch_design_doc(repo_remote_url, doc_path, git_ref).await
    }

    async fn plan(&self, input: &PlannerInput) -> PlannerOutcome {
        Planner::plan(self.api_key.as_deref(), input).await
    }
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// The terminal result of one [`Populator::run`] pass. Mirrors the
/// `planner_runs.outcome` recorded for the run, so tests can assert the pass
/// took the expected branch. `run_id` is `None` only when no row was claimed
/// (a claim conflict or a claim error).
#[derive(Debug, PartialEq, Eq)]
pub enum PopulateOutcome {
    /// A prior live `planner_runs` row already exists — another trigger,
    /// poller restart, or manual retry owns (or already completed) this
    /// project. Clean skip, nothing written.
    SkippedAlreadyPopulated,
    /// The project already has implementation tasks (operator pre-seeded).
    /// Refuse-not-merge; attention item raised.
    SkippedPreSeeded { existing: usize },
    /// The design doc had no task-breakdown section. Clean no-op.
    NoBreakdown,
    /// A breakdown section was present but yielded no tasks. No-op.
    EmptyBreakdown,
    /// Proposal exceeded the cap; rejected whole (never truncated).
    RejectedTooMany { count: usize, max: usize },
    /// Proposal's graph was cyclic, or referenced an unknown/duplicate
    /// handle. Rejected whole.
    RejectedBadGraph,
    /// The design doc pointer is unset or the file 404'd at the merged ref.
    DocMissing,
    /// The doc fetch exhausted its retries (transient GitHub / transport).
    FetchFailed,
    /// The Planner failed (no API key, API error, transport, or schema-
    /// invalid output) after its own bounded retry.
    PlannerFailed,
    /// Tasks were created (staged, `autostart = false`) and graph-wired.
    Staged {
        created: usize,
        edges: usize,
        low_confidence: bool,
    },
    /// An internal DB error prevented the pass from claiming or completing.
    Errored,
}

impl PopulateOutcome {
    /// Short tag for logs.
    pub fn tag(&self) -> &'static str {
        match self {
            PopulateOutcome::SkippedAlreadyPopulated => "skipped_already_populated",
            PopulateOutcome::SkippedPreSeeded { .. } => "skipped_pre_seeded",
            PopulateOutcome::NoBreakdown => "no_breakdown",
            PopulateOutcome::EmptyBreakdown => "empty_breakdown",
            PopulateOutcome::RejectedTooMany { .. } => "rejected_too_many",
            PopulateOutcome::RejectedBadGraph => "rejected_cycle",
            PopulateOutcome::DocMissing => "doc_missing",
            PopulateOutcome::FetchFailed => "fetch_failed",
            PopulateOutcome::PlannerFailed => "planner_failed",
            PopulateOutcome::Staged { .. } => "staged",
            PopulateOutcome::Errored => "errored",
        }
    }
}

// ---------------------------------------------------------------------------
// The orchestrator
// ---------------------------------------------------------------------------

/// The Populator. A zero-sized entry point; the pass holds no state.
pub struct Populator;

impl Populator {
    /// Run one populate pass for the project named in `ctx`.
    ///
    /// This is the fully-testable core: `steps` injects the two network
    /// operations, `db` is any [`WorkDb`] (an in-memory one in tests), and
    /// `max_tasks` is the guardrail cap. Returns the [`PopulateOutcome`] that
    /// mirrors the recorded `planner_runs.outcome`.
    ///
    /// Ordering matches design §"Architecture overview":
    ///
    /// 1. **Claim** the project (`planner_runs` insert). Conflict → skip.
    /// 2. **Pre-seeded check** — refuse if the project already has
    ///    implementation tasks.
    /// 3. **Gather context** — project, product, existing task names, and the
    ///    merged design-doc pointer.
    /// 4. **Fetch** the doc live at the merged ref.
    /// 5. **Plan** (the one LLM step).
    /// 6. **Validate** the structured proposal.
    /// 7. **Apply** via the Materializer (the single write transaction).
    /// 8. **Audit + surface** — update the `planner_runs` row and raise an
    ///    attention item.
    pub async fn run(
        db: &WorkDb,
        steps: &dyn PopulatorSteps,
        ctx: &PopulateContext,
        max_tasks: usize,
    ) -> PopulateOutcome {
        // 1. Idempotency claim. The UNIQUE-per-project partial index makes
        //    this the circuit breaker: at most one live row per project.
        let run = match db.claim_planner_run(ClaimPlannerRunInput {
            project_id: &ctx.project_id,
            product_id: &ctx.product_id,
            design_task_id: Some(&ctx.design_task_id),
            caller: CALLER_MERGE_TRIGGER,
        }) {
            Ok(Some(run)) => run,
            Ok(None) => {
                tracing::info!(
                    project_id = %ctx.project_id,
                    pr_url = %ctx.pr_url,
                    "populator: skipped — project already has a live planner run (already populated or in flight)",
                );
                return PopulateOutcome::SkippedAlreadyPopulated;
            }
            Err(err) => {
                tracing::warn!(project_id = %ctx.project_id, ?err, "populator: failed to claim planner run");
                return PopulateOutcome::Errored;
            }
        };
        let run_id = run.id;

        // From here every early return records a terminal outcome on the
        // claimed row (releasing the idempotency gate for a later re-plan)
        // and surfaces an attention item.
        match Self::run_claimed(db, steps, ctx, &run_id, max_tasks).await {
            Ok(outcome) => outcome,
            Err(err) => {
                // Internal DB error after the claim. Record it so the row is
                // not stranded `running`.
                tracing::warn!(project_id = %ctx.project_id, run_id = %run_id, ?err, "populator: internal error");
                let _ = db.update_planner_run(
                    &run_id,
                    PlannerRunPatch::builder()
                        .outcome(PLANNER_OUTCOME_PLANNER_FAILED)
                        .result_summary(format!("internal error: {err}"))
                        .build(),
                );
                PopulateOutcome::Errored
            }
        }
    }

    /// The body after a successful claim. Split out so the claim path can map
    /// any propagated error to a recorded `planner_failed` row.
    async fn run_claimed(
        db: &WorkDb,
        steps: &dyn PopulatorSteps,
        ctx: &PopulateContext,
        run_id: &str,
        max_tasks: usize,
    ) -> anyhow::Result<PopulateOutcome> {
        // 2. Pre-seeded refusal. Belt-and-suspenders beyond the claim gate:
        //    if the operator already put implementation tasks here, the
        //    Planner cannot reason about *why*, so merging risks duplicate or
        //    contradictory edges. Refuse and surface the `--force` escape.
        //    Counts *any* non-design kind (not just project_task/investigation)
        //    so the "any non-design task" contract holds even for a stray
        //    `task`/`revision`/`followup` row on the project.
        let project_tasks = db.list_project_task_briefs(&ctx.project_id)?;
        let existing_impl = project_tasks
            .iter()
            .filter(|(_, _, kind)| kind != TaskKind::Design.as_str())
            .count();
        if existing_impl > 0 {
            let n = existing_impl;
            tracing::info!(
                project_id = %ctx.project_id,
                existing = n,
                "populator: refusing — project already has implementation task(s) (pre-seeded)",
            );
            Self::finish(
                db,
                ctx,
                run_id,
                PlannerRunPatch::builder()
                    .outcome(PLANNER_OUTCOME_SKIPPED_PRE_SEEDED)
                    .result_summary(format!("refused: project already has {n} implementation task(s)"))
                    .build(),
                "Auto-populate skipped: project already has tasks",
                format!(
                    "Skipped auto-populate of this project: it already has {n} implementation task(s). \
                     The design PR merged, but the project was pre-seeded. Run \
                     `boss project plan <project> --force` to add the planner's tasks anyway \
                     (existing tasks are preserved by name dedup)."
                ),
            );
            return Ok(PopulateOutcome::SkippedPreSeeded { existing: n });
        }

        // 3. Gather context. The design-doc pointer was written by
        //    `on_design_pr_merged` before this pass; read it back.
        let project = db.get_project(&ctx.project_id)?;
        let product = db.get_product(&ctx.product_id)?;

        let Some(doc_path) = project.design_doc_path.clone() else {
            tracing::warn!(project_id = %ctx.project_id, "populator: project has no design_doc_path pointer");
            Self::finish(
                db,
                ctx,
                run_id,
                PlannerRunPatch::builder()
                    .outcome(PLANNER_OUTCOME_DOC_MISSING)
                    .result_summary("project has no design_doc_path pointer")
                    .build(),
                "Auto-populate failed: no design doc pointer",
                "The design PR merged but no design-doc path was recorded for this project, so the \
                 planner has nothing to read. Set the pointer and re-run `boss project plan <project>`."
                    .to_owned(),
            );
            return Ok(PopulateOutcome::DocMissing);
        };

        // Repo the doc lives in: the design-doc pointer's repo (which may be a
        // docs-site override) if set, else the product's primary repo.
        let repo_remote_url = project
            .design_doc_repo_remote_url
            .clone()
            .or_else(|| product.as_ref().and_then(|p| p.repo_remote_url.clone()));
        let Some(repo_remote_url) = repo_remote_url else {
            tracing::warn!(project_id = %ctx.project_id, "populator: no repo_remote_url to fetch the doc from");
            Self::finish(
                db,
                ctx,
                run_id,
                PlannerRunPatch::builder()
                    .outcome(PLANNER_OUTCOME_DOC_MISSING)
                    .result_summary("no repo_remote_url resolvable for the design doc")
                    .build(),
                "Auto-populate failed: no repo for design doc",
                "The design PR merged but neither the project's design-doc pointer nor its product \
                 resolves to a repository URL, so the planner cannot fetch the doc."
                    .to_owned(),
            );
            return Ok(PopulateOutcome::DocMissing);
        };
        let git_ref = project
            .design_doc_branch
            .clone()
            .unwrap_or_else(|| DEFAULT_DOC_REF.to_owned());

        let doc_ref = DocRef {
            repo_remote_url: repo_remote_url.clone(),
            git_ref: git_ref.clone(),
            path: doc_path.clone(),
        };
        let doc_ref_summary = format!("{repo_remote_url}|{git_ref}|{doc_path}");

        // 4. Fetch the doc live at the merged ref.
        let doc = match steps.fetch_doc(&repo_remote_url, &doc_path, &git_ref).await {
            DocFetchOutcome::Content(text) => text,
            DocFetchOutcome::DocMissing => {
                tracing::warn!(project_id = %ctx.project_id, doc_path, git_ref, "populator: design doc 404 at merged ref");
                Self::finish(
                    db,
                    ctx,
                    run_id,
                    PlannerRunPatch::builder()
                        .outcome(PLANNER_OUTCOME_DOC_MISSING)
                        .doc_ref(doc_ref_summary.clone())
                        .result_summary(format!("design doc not found at {git_ref}: {doc_path}"))
                        .build(),
                    "Auto-populate failed: design doc not found",
                    format!(
                        "The design doc `{doc_path}` was not found at `{git_ref}` when the planner \
                         tried to read it (it may have moved after merge). No tasks were created. \
                         Re-run `boss project plan <project>` once the path is correct."
                    ),
                );
                return Ok(PopulateOutcome::DocMissing);
            }
            DocFetchOutcome::FetchFailed { reason } => {
                tracing::warn!(project_id = %ctx.project_id, reason, "populator: doc fetch failed");
                Self::finish(
                    db,
                    ctx,
                    run_id,
                    PlannerRunPatch::builder()
                        .outcome(PLANNER_OUTCOME_FETCH_FAILED)
                        .doc_ref(doc_ref_summary.clone())
                        .result_summary(format!("doc fetch failed: {reason}"))
                        .build(),
                    "Auto-populate failed: could not fetch design doc",
                    format!(
                        "The planner could not fetch the design doc from GitHub after retries \
                         ({reason}). No tasks were created. Re-run `boss project plan <project>` \
                         once GitHub is reachable."
                    ),
                );
                return Ok(PopulateOutcome::FetchFailed);
            }
        };

        // Assemble the Planner input. Existing task names are a dedup hint;
        // design tasks are excluded (the merged design already exists, and the
        // Planner must never re-propose it). Filtering on `kind != design`
        // drops the design task without needing its id.
        let existing_tasks: Vec<TaskBrief> = project_tasks
            .iter()
            .filter(|(_, _, kind)| kind != TaskKind::Design.as_str())
            .map(|(id, name, _)| TaskBrief {
                id: id.clone(),
                name: name.clone(),
            })
            .collect();
        let input_summary = format!(
            "doc_len={} chars, project={}, product={}, existing_tasks={}, max_tasks={}",
            doc.len(),
            project.slug,
            product.as_ref().map(|p| p.slug.as_str()).unwrap_or("?"),
            existing_tasks.len(),
            max_tasks,
        );

        let planner_input = PlannerInput::builder()
            .design_doc(doc)
            .design_doc_ref(doc_ref)
            .project(ProjectContext {
                id: project.id.clone(),
                name: project.name.clone(),
                slug: project.slug.clone(),
                description: project.description.clone(),
                goal: project.goal.clone(),
            })
            .product(ProductContext {
                id: ctx.product_id.clone(),
                slug: product.as_ref().map(|p| p.slug.clone()).unwrap_or_default(),
                name: product.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
                repo_remote_url,
            })
            .existing_tasks(existing_tasks)
            .max_tasks(max_tasks)
            .build();

        // Record what we're about to send before the (slow) LLM call, so the
        // audit row is informative even if the process dies mid-call.
        let _ = db.update_planner_run(
            run_id,
            PlannerRunPatch::builder()
                .doc_ref(doc_ref_summary.clone())
                .model(PLANNER_MODEL)
                .input_summary(input_summary)
                .build(),
        );

        // 5. Plan (the one LLM step).
        let output = match steps.plan(&planner_input).await {
            PlannerOutcome::Success(output) => output,
            failure => {
                let detail = failure.detail();
                tracing::warn!(project_id = %ctx.project_id, outcome = failure.tag(), detail, "populator: planner failed");
                Self::finish(
                    db,
                    ctx,
                    run_id,
                    PlannerRunPatch::builder()
                        .outcome(PLANNER_OUTCOME_PLANNER_FAILED)
                        .result_summary(format!("planner {}: {detail}", failure.tag()))
                        .build(),
                    "Auto-populate failed: planner error",
                    format!(
                        "The planner could not produce a task graph ({detail}). No tasks were \
                         created. Re-run `boss project plan <project>` (configure ANTHROPIC_API_KEY \
                         first if that is the cause)."
                    ),
                );
                return Ok(PopulateOutcome::PlannerFailed);
            }
        };

        // Persist the raw structured output + rationale + effort audit before
        // validating, so the operator can always read exactly what the model
        // proposed (design §"Durable audit trail").
        let raw_output = serde_json::to_string(&output).unwrap_or_else(|e| format!("<unserializable: {e}>"));
        let _ = db.update_planner_run(
            run_id,
            PlannerRunPatch::builder()
                .raw_output(raw_output)
                .notes(output.notes.clone())
                .effort_audit(output.effort_audit.join("\n"))
                .build(),
        );

        // 6. Validate the structured proposal.
        match validate(&output, max_tasks) {
            ValidationResult::NoBreakdown => {
                Self::finish(
                    db,
                    ctx,
                    run_id,
                    PlannerRunPatch::builder()
                        .outcome(PLANNER_OUTCOME_NO_BREAKDOWN)
                        .result_summary("no task-breakdown section in the doc")
                        .build(),
                    "Auto-populate: no task breakdown found",
                    "The design doc for this project had no implementation task-breakdown section, \
                     so no tasks were created. Plan manually, or add a breakdown and re-run \
                     `boss project plan <project>`."
                        .to_owned(),
                );
                Ok(PopulateOutcome::NoBreakdown)
            }
            ValidationResult::EmptyBreakdown => {
                Self::finish(
                    db,
                    ctx,
                    run_id,
                    PlannerRunPatch::builder()
                        .outcome(PLANNER_OUTCOME_NO_BREAKDOWN)
                        .result_summary("breakdown section present but no tasks extracted")
                        .build(),
                    "Auto-populate: empty task breakdown",
                    "The planner found a task-breakdown section but extracted no tasks from it. \
                     No tasks were created. Re-run `boss project plan <project>` or plan manually."
                        .to_owned(),
                );
                Ok(PopulateOutcome::EmptyBreakdown)
            }
            ValidationResult::RejectedTooMany { count, max } => {
                Self::finish(
                    db,
                    ctx,
                    run_id,
                    PlannerRunPatch::builder()
                        .outcome(PLANNER_OUTCOME_REJECTED_TOO_MANY)
                        .result_summary(format!("rejected: proposed {count} tasks, cap is {max}"))
                        .build(),
                    "Auto-populate rejected: too many tasks",
                    format!(
                        "The planner proposed {count} tasks, over the cap of {max}. The whole \
                         proposal was rejected (nothing is silently truncated) and no tasks were \
                         created. Split the project, or re-run `boss project plan <project>` with a \
                         higher cap."
                    ),
                );
                Ok(PopulateOutcome::RejectedTooMany { count, max })
            }
            ValidationResult::RejectedDuplicateHandle { handle } => {
                Self::reject_bad_graph(db, ctx, run_id, format!("duplicate task handle: {handle}"));
                Ok(PopulateOutcome::RejectedBadGraph)
            }
            ValidationResult::RejectedUnknownHandle { handle } => {
                Self::reject_bad_graph(db, ctx, run_id, format!("edge references unknown handle: {handle}"));
                Ok(PopulateOutcome::RejectedBadGraph)
            }
            ValidationResult::RejectedCycle { cycle } => {
                Self::reject_bad_graph(db, ctx, run_id, format!("dependency cycle: {}", cycle.join(" → ")));
                Ok(PopulateOutcome::RejectedBadGraph)
            }
            ValidationResult::Valid { low_confidence } => {
                Self::apply_and_stage(db, ctx, run_id, &output, low_confidence)
            }
        }
    }

    /// 7 + 8. Materialize a valid proposal (the single write transaction),
    /// then audit and surface. The tasks are created staged
    /// (`autostart = false`); an operator releases them to begin dispatch.
    fn apply_and_stage(
        db: &WorkDb,
        ctx: &PopulateContext,
        run_id: &str,
        output: &PlannerOutput,
        low_confidence: bool,
    ) -> anyhow::Result<PopulateOutcome> {
        let result = match Materializer::apply(db, &ctx.project_id, run_id, output) {
            Ok(result) => result,
            Err(err) => {
                // The apply transaction is all-or-nothing: on error nothing
                // was written (no partial graph). Validation already passed on
                // this path, so an apply failure is not a proposal-graph defect
                // the operator can fix by re-planning — it is an apply-time
                // failure (a transient DB error, a same-product violation, or a
                // cycle formed against *existing* project edges that in-memory
                // validation could not see). Record it under `planner_failed`
                // with the real error rather than mislabelling it a malformed
                // graph, and surface an accurate attention item.
                tracing::warn!(project_id = %ctx.project_id, run_id, ?err, "populator: materializer apply failed");
                Self::finish(
                    db,
                    ctx,
                    run_id,
                    PlannerRunPatch::builder()
                        .outcome(PLANNER_OUTCOME_PLANNER_FAILED)
                        .result_summary(format!("apply failed: {err}"))
                        .build(),
                    "Auto-populate failed: could not apply task graph",
                    format!(
                        "The planner's proposal passed validation but could not be applied \
                         ({err}). No tasks were created (the apply is transactional). Re-run \
                         `boss project plan <project>`."
                    ),
                );
                return Ok(PopulateOutcome::PlannerFailed);
            }
        };

        let created = result.created.len();
        let edges = result.edges_created;
        let skipped = result.skipped.len();
        let summary = format!("staged {created} task(s), {edges} edge(s), {skipped} deduped");
        tracing::info!(
            project_id = %ctx.project_id,
            run_id,
            created,
            edges,
            skipped,
            low_confidence,
            "populator: staged tasks",
        );

        let (title, body) = if low_confidence {
            (
                "Auto-populate: review staged tasks (low confidence)",
                format!(
                    "Auto-populated {created} staged task(s) and {edges} dependency edge(s) from the \
                     merged design doc, but the planner flagged **low confidence** in the plan. \
                     Scrutinise the tasks on the kanban, then run `boss project release <project>` to \
                     begin dispatch (or `boss project unpopulate <project>` to undo)."
                ),
            )
        } else {
            (
                "Auto-populate: review & release staged tasks",
                format!(
                    "Auto-populated {created} staged task(s) and {edges} dependency edge(s) from the \
                     merged design doc. They are staged (not dispatching). Review them on the kanban, \
                     then run `boss project release <project>` to begin dispatch (or \
                     `boss project unpopulate <project>` to undo)."
                ),
            )
        };

        Self::finish(
            db,
            ctx,
            run_id,
            PlannerRunPatch::builder()
                .outcome(PLANNER_OUTCOME_STAGED)
                .result_summary(summary)
                .build(),
            title,
            body,
        );

        Ok(PopulateOutcome::Staged {
            created,
            edges,
            low_confidence,
        })
    }

    /// Record a rejected-graph outcome (`rejected_cycle` bucket covers cyclic,
    /// unknown-handle, and duplicate-handle proposals per the design) plus an
    /// attention item. Nothing was written.
    fn reject_bad_graph(db: &WorkDb, ctx: &PopulateContext, run_id: &str, reason: String) {
        tracing::warn!(project_id = %ctx.project_id, run_id, reason, "populator: rejected malformed task graph");
        Self::finish(
            db,
            ctx,
            run_id,
            PlannerRunPatch::builder()
                .outcome(PLANNER_OUTCOME_REJECTED_CYCLE)
                .result_summary(format!("rejected: {reason}"))
                .build(),
            "Auto-populate rejected: malformed task graph",
            format!(
                "The planner's proposed task graph was rejected as malformed ({reason}). No tasks \
                 were created. Re-run `boss project plan <project>` or plan manually."
            ),
        );
    }

    /// Update the claimed `planner_runs` row with the terminal patch and raise
    /// the operator-facing attention item. Both are best-effort: a failure to
    /// surface must not itself panic or fail the pass (the pass already did
    /// the right thing to the DB / is no-op).
    fn finish(db: &WorkDb, ctx: &PopulateContext, run_id: &str, patch: PlannerRunPatch, title: &str, body: String) {
        if let Err(err) = db.update_planner_run(run_id, patch) {
            tracing::warn!(project_id = %ctx.project_id, run_id, ?err, "populator: failed to update planner run");
        }
        if let Err(err) = db.create_attention_item(
            CreateAttentionItemInput::builder()
                .kind(ATTENTION_KIND)
                .title(title)
                .body_markdown(body)
                .work_item_id(ctx.design_task_id.clone())
                .status("open")
                .build(),
        ) {
            tracing::warn!(project_id = %ctx.project_id, ?err, "populator: failed to raise attention item");
        }
    }
}

// ---------------------------------------------------------------------------
// Production wiring: install-once capability + background enqueue
// ---------------------------------------------------------------------------

/// Startup-installed configuration for the auto-populate feature. Held in a
/// process-wide [`OnceLock`] so the merge-trigger hook (deep in the poller's
/// call chain, which only has a `&WorkDb`) can enqueue a populate without
/// threading the api key and cap through every poller signature. Contexts
/// that never [`install`] it — the merge-poller unit tests, non-server
/// callers — make [`enqueue_from_merge`] a no-op, so no test reaches the
/// network.
pub struct PopulatorConfig {
    /// Anthropic API key captured from config at engine startup.
    pub api_key: Option<String>,
    /// Hard cap on tasks per populate.
    pub max_tasks: usize,
}

static POPULATOR: OnceLock<PopulatorConfig> = OnceLock::new();

/// Install the auto-populate capability. Called once at engine startup
/// (`app::server`). Idempotent: a second call is ignored.
pub fn install(config: PopulatorConfig) {
    if POPULATOR.set(config).is_err() {
        tracing::debug!("populator: install called more than once; keeping the first config");
    }
}

/// Enqueue a background populate for a just-merged design PR.
///
/// Called from `merge_poller::mark_merged`. Cheap and synchronous: it clones
/// the (cheap) [`WorkDb`] handle and spawns the multi-second pass on a tokio
/// task so the poller loop never blocks on the LLM call. A no-op when the
/// capability has not been [`install`]ed (tests / non-server contexts).
pub fn enqueue_from_merge(work_db: &WorkDb, ctx: PopulateContext) {
    let Some(config) = POPULATOR.get() else {
        tracing::debug!(project_id = %ctx.project_id, "populator: not installed; skipping auto-populate enqueue");
        return;
    };
    let db = work_db.clone();
    let steps = LivePopulatorSteps {
        api_key: config.api_key.clone(),
    };
    let max_tasks = config.max_tasks;
    tracing::info!(project_id = %ctx.project_id, pr_url = %ctx.pr_url, "populator: enqueuing auto-populate");
    tokio::spawn(async move {
        let outcome = Populator::run(&db, &steps, &ctx, max_tasks).await;
        tracing::info!(
            project_id = %ctx.project_id,
            outcome = outcome.tag(),
            "populator: auto-populate pass complete",
        );
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use boss_protocol::{
        Confidence, CreateProductInput, CreateProjectInput, CreateTaskInput, EffortLevel, ProposedEdge, ProposedTask,
    };

    use crate::work::WorkDb;

    // ---- fixtures ----------------------------------------------------------

    fn open() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    /// Create a product + project + a `kind=design` task, with the project's
    /// design-doc pointer set (as `on_design_pr_merged` would leave it).
    /// Returns `(product_id, project_id, design_task_id)`.
    fn seed(db: &WorkDb) -> (String, String, String) {
        let product = db
            .create_product(
                CreateProductInput::builder()
                    .name("Boss")
                    .repo_remote_url("git@github.com:owner/repo.git")
                    .build(),
            )
            .unwrap();
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Alpha")
                    .goal("build it")
                    .build(),
            )
            .unwrap();
        // `create_project` auto-creates the project's `kind = 'design'` task
        // at ordinal 0; find it rather than creating a second one.
        let design_id = db
            .list_tasks(&product.id, Some(&project.id), None, false)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == TaskKind::Design)
            .expect("project should have an auto-created design task")
            .id;
        db.set_project_design_doc(boss_protocol::SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_path: Some("tools/boss/docs/designs/alpha.md".to_owned()),
            design_doc_branch: Some("main".to_owned()),
            design_doc_repo_remote_url: Some("git@github.com:owner/repo.git".to_owned()),
            unset: false,
        })
        .unwrap();
        (product.id, project.id, design_id)
    }

    fn ctx(product_id: &str, project_id: &str, design_id: &str) -> PopulateContext {
        PopulateContext {
            project_id: project_id.to_owned(),
            product_id: product_id.to_owned(),
            design_task_id: design_id.to_owned(),
            pr_url: "https://github.com/owner/repo/pull/1".to_owned(),
        }
    }

    /// A configurable fake for the two network steps.
    struct FakeSteps {
        doc: DocFetchOutcomeKind,
        plan: PlannerOutcomeKind,
    }

    /// Cloneable descriptors the fake maps to real (non-Clone) outcome enums.
    enum DocFetchOutcomeKind {
        Content(String),
        Missing,
        Failed,
    }
    enum PlannerOutcomeKind {
        Success(PlannerOutput),
        NoApiKey,
    }

    #[async_trait]
    impl PopulatorSteps for FakeSteps {
        async fn fetch_doc(&self, _repo: &str, _path: &str, _git_ref: &str) -> DocFetchOutcome {
            match &self.doc {
                DocFetchOutcomeKind::Content(s) => DocFetchOutcome::Content(s.clone()),
                DocFetchOutcomeKind::Missing => DocFetchOutcome::DocMissing,
                DocFetchOutcomeKind::Failed => DocFetchOutcome::FetchFailed {
                    reason: "boom".to_owned(),
                },
            }
        }

        async fn plan(&self, _input: &PlannerInput) -> PlannerOutcome {
            match &self.plan {
                PlannerOutcomeKind::Success(out) => PlannerOutcome::Success(out.clone()),
                PlannerOutcomeKind::NoApiKey => PlannerOutcome::NoApiKey,
            }
        }
    }

    fn ptask(handle: &str, name: &str) -> ProposedTask {
        ProposedTask {
            handle: handle.to_owned(),
            name: name.to_owned(),
            description: format!(
                "Do {name}.\n\n[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\""
            ),
            kind: TaskKind::ProjectTask,
            effort: EffortLevel::Small,
            ordinal: 0,
        }
    }

    fn plan_output(
        tasks: Vec<ProposedTask>,
        edges: Vec<ProposedEdge>,
        confidence: Confidence,
        breakdown_found: bool,
    ) -> PlannerOutput {
        PlannerOutput {
            tasks,
            edges,
            confidence,
            breakdown_found,
            notes: "rationale".to_owned(),
            effort_audit: vec!["[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\"".to_owned()],
        }
    }

    fn steps_with(doc: DocFetchOutcomeKind, plan: PlannerOutcomeKind) -> FakeSteps {
        FakeSteps { doc, plan }
    }

    fn open_attention_count(db: &WorkDb, design_id: &str) -> usize {
        db.list_attention_items_for_work_item(design_id)
            .unwrap()
            .into_iter()
            .filter(|a| a.kind == ATTENTION_KIND && a.status == "open")
            .count()
    }

    fn project_task_count(db: &WorkDb, product_id: &str, project_id: &str) -> usize {
        db.list_tasks(product_id, Some(project_id), None, false)
            .unwrap()
            .into_iter()
            .filter(|t| t.kind != TaskKind::Design)
            .count()
    }

    // ---- happy path: stage tasks ------------------------------------------

    #[tokio::test]
    async fn stages_tasks_on_valid_plan() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let output = plan_output(
            vec![ptask("schema", "Add schema"), ptask("engine", "Engine handler")],
            vec![ProposedEdge {
                dependent: "engine".to_owned(),
                prerequisite: "schema".to_owned(),
            }],
            Confidence::High,
            true,
        );
        let steps = steps_with(
            DocFetchOutcomeKind::Content("# doc".to_owned()),
            PlannerOutcomeKind::Success(output),
        );

        let outcome = Populator::run(
            &db,
            &steps,
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert_eq!(
            outcome,
            PopulateOutcome::Staged {
                created: 2,
                edges: 1,
                low_confidence: false
            }
        );

        // Two staged (autostart=false) tasks now exist in the project.
        assert_eq!(project_task_count(&db, &product_id, &project_id), 2);
        // The planner run landed on `staged`.
        let run = db.live_planner_run_for_project(&project_id).unwrap().unwrap();
        assert_eq!(run.outcome, PLANNER_OUTCOME_STAGED);
        assert!(run.raw_output.is_some(), "raw output persisted for audit");
        assert_eq!(run.model.as_deref(), Some(PLANNER_MODEL));
        // An attention item was raised.
        assert_eq!(open_attention_count(&db, &design_id), 1);
    }

    #[tokio::test]
    async fn low_confidence_still_stages_and_flags() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let output = plan_output(vec![ptask("a", "Task A")], vec![], Confidence::Low, true);
        let steps = steps_with(
            DocFetchOutcomeKind::Content("# doc".to_owned()),
            PlannerOutcomeKind::Success(output),
        );
        let outcome = Populator::run(
            &db,
            &steps,
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert_eq!(
            outcome,
            PopulateOutcome::Staged {
                created: 1,
                edges: 0,
                low_confidence: true
            }
        );
        assert_eq!(open_attention_count(&db, &design_id), 1);
    }

    // ---- idempotency -------------------------------------------------------

    #[tokio::test]
    async fn second_run_skips_already_populated() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let output = plan_output(vec![ptask("a", "Task A")], vec![], Confidence::High, true);
        let first = Populator::run(
            &db,
            &steps_with(
                DocFetchOutcomeKind::Content("# doc".to_owned()),
                PlannerOutcomeKind::Success(output.clone()),
            ),
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert!(matches!(first, PopulateOutcome::Staged { .. }));

        // A second trigger for the same project must skip — the staged row
        // holds the idempotency gate.
        let second = Populator::run(
            &db,
            &steps_with(
                DocFetchOutcomeKind::Content("# doc".to_owned()),
                PlannerOutcomeKind::Success(output),
            ),
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert_eq!(second, PopulateOutcome::SkippedAlreadyPopulated);
        // Still exactly the first batch — no duplicates.
        assert_eq!(project_task_count(&db, &product_id, &project_id), 1);
    }

    // ---- pre-seeded refusal ------------------------------------------------

    #[tokio::test]
    async fn refuses_pre_seeded_project() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        // Operator pre-seeded an implementation task.
        db.create_task(
            CreateTaskInput::builder()
                .product_id(product_id.clone())
                .project_id(project_id.clone())
                .name("Hand-written task")
                .build(),
        )
        .unwrap();

        let steps = steps_with(
            DocFetchOutcomeKind::Content("# doc".to_owned()),
            PlannerOutcomeKind::Success(plan_output(vec![ptask("a", "Task A")], vec![], Confidence::High, true)),
        );
        let outcome = Populator::run(
            &db,
            &steps,
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert_eq!(outcome, PopulateOutcome::SkippedPreSeeded { existing: 1 });
        // The planner's task was NOT added; only the pre-seeded one remains.
        assert_eq!(project_task_count(&db, &product_id, &project_id), 1);
        assert_eq!(open_attention_count(&db, &design_id), 1);
        // The claimed row went terminal (gate released for a `--force` replan).
        assert!(db.live_planner_run_for_project(&project_id).unwrap().is_none());
    }

    // ---- fetch / plan failures leave nothing behind ------------------------

    #[tokio::test]
    async fn doc_missing_is_a_clean_no_op() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let steps = steps_with(DocFetchOutcomeKind::Missing, PlannerOutcomeKind::NoApiKey);
        let outcome = Populator::run(
            &db,
            &steps,
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert_eq!(outcome, PopulateOutcome::DocMissing);
        assert_eq!(project_task_count(&db, &product_id, &project_id), 0);
        assert_eq!(open_attention_count(&db, &design_id), 1);
    }

    #[tokio::test]
    async fn fetch_failure_is_recorded() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let steps = steps_with(DocFetchOutcomeKind::Failed, PlannerOutcomeKind::NoApiKey);
        let outcome = Populator::run(
            &db,
            &steps,
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert_eq!(outcome, PopulateOutcome::FetchFailed);
        assert_eq!(project_task_count(&db, &product_id, &project_id), 0);
    }

    #[tokio::test]
    async fn planner_no_api_key_is_recorded() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let steps = steps_with(
            DocFetchOutcomeKind::Content("# doc".to_owned()),
            PlannerOutcomeKind::NoApiKey,
        );
        let outcome = Populator::run(
            &db,
            &steps,
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert_eq!(outcome, PopulateOutcome::PlannerFailed);
        assert_eq!(project_task_count(&db, &product_id, &project_id), 0);
        assert_eq!(open_attention_count(&db, &design_id), 1);
    }

    // ---- validation rejections --------------------------------------------

    #[tokio::test]
    async fn no_breakdown_is_a_clean_no_op() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let output = plan_output(vec![], vec![], Confidence::High, false);
        let steps = steps_with(
            DocFetchOutcomeKind::Content("# doc".to_owned()),
            PlannerOutcomeKind::Success(output),
        );
        let outcome = Populator::run(
            &db,
            &steps,
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert_eq!(outcome, PopulateOutcome::NoBreakdown);
        assert_eq!(project_task_count(&db, &product_id, &project_id), 0);
        // The run is terminal (no_breakdown is not a live outcome).
        assert!(db.live_planner_run_for_project(&project_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn over_cap_rejects_whole_proposal() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let tasks: Vec<_> = (0..3).map(|i| ptask(&format!("h{i}"), &format!("Task {i}"))).collect();
        let output = plan_output(tasks, vec![], Confidence::High, true);
        let steps = steps_with(
            DocFetchOutcomeKind::Content("# doc".to_owned()),
            PlannerOutcomeKind::Success(output),
        );
        // Cap of 2, proposal of 3.
        let outcome = Populator::run(&db, &steps, &ctx(&product_id, &project_id, &design_id), 2).await;
        assert_eq!(outcome, PopulateOutcome::RejectedTooMany { count: 3, max: 2 });
        assert_eq!(project_task_count(&db, &product_id, &project_id), 0);
        assert_eq!(open_attention_count(&db, &design_id), 1);
    }

    #[tokio::test]
    async fn cyclic_proposal_is_rejected() {
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        let output = plan_output(
            vec![ptask("a", "Task A"), ptask("b", "Task B")],
            vec![
                ProposedEdge {
                    dependent: "a".to_owned(),
                    prerequisite: "b".to_owned(),
                },
                ProposedEdge {
                    dependent: "b".to_owned(),
                    prerequisite: "a".to_owned(),
                },
            ],
            Confidence::High,
            true,
        );
        let steps = steps_with(
            DocFetchOutcomeKind::Content("# doc".to_owned()),
            PlannerOutcomeKind::Success(output),
        );
        let outcome = Populator::run(
            &db,
            &steps,
            &ctx(&product_id, &project_id, &design_id),
            DEFAULT_MAX_TASKS,
        )
        .await;
        assert_eq!(outcome, PopulateOutcome::RejectedBadGraph);
        assert_eq!(project_task_count(&db, &product_id, &project_id), 0);
        assert_eq!(open_attention_count(&db, &design_id), 1);
    }

    // ---- enqueue is a no-op when not installed -----------------------------

    #[tokio::test]
    async fn enqueue_is_noop_when_not_installed() {
        // The global is never installed in unit tests, so this must not spawn
        // a populate (and must not panic without a config).
        let db = open();
        let (product_id, project_id, design_id) = seed(&db);
        enqueue_from_merge(&db, ctx(&product_id, &project_id, &design_id));
        // No planner run was claimed.
        assert!(db.live_planner_run_for_project(&project_id).unwrap().is_none());
    }
}
