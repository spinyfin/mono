//! Apply pipeline for auto-apply worker-proposal kinds.
//!
//! Implementation task 5 of `worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! ("Apply pipeline: auto-apply kinds") built a policy table plus
//! synchronous appliers for `attention`, `effort_escalation`, `blocked`, and
//! `deferred_scope` — each writing the same `work_attention_items` rows
//! today's marker detectors write ([`crate::completion::WorkerCompletionHandler::file_worker_signal_attention`],
//! [`crate::completion::WorkerCompletionHandler::record_deferred_scope_item`]),
//! so a submitted proposal and its effect are the same event.
//!
//! Implementation task 6 ("Apply pipeline: gated `followup_task` + verified
//! `automation_outcome` / `pr_created`") extends this same module:
//!
//! - `followup_task` stays [`ProposalApplyPolicy::Gated`] — task creation
//!   still requires the human batch-accept gesture
//!   ([`crate::work::attentions::action_attention_group`]) — but
//!   [`stage_followup_task_in_transaction`] runs unconditionally at
//!   submission (regardless of gating) to upsert the member into the
//!   originating task's `followup` attention group: "gated" means the
//!   *task* is gated, not the group membership the human needs to see it.
//! - `automation_outcome` and `pr_created` flip to
//!   [`ProposalApplyPolicy::AutoApply`], with real appliers below.
//!
//! [`apply_policy`] is a plain code table, not a DB row or feature flag: per
//! the design's risk note ("If auto-applied attention proposals get noisy,
//! the policy table can flip that kind to gated without schema change"),
//! flipping a kind's disposition is a one-line code change here, not a
//! migration.
//!
//! [`apply_in_transaction`] runs inside [`WorkDb::submit_worker_proposal`]'s
//! existing `Immediate` transaction, so the produced row commits or rolls
//! back together with the `worker_proposals` row it is `applied_ref`-linked
//! from — "proposal accepted" and "effect exists" are the same commit.
//! Unlike task 5's appliers (which always succeed once their payload
//! deserializes), `automation_outcome` and `pr_created` can legitimately
//! *refuse* a syntactically-valid payload — a provenance mismatch, a wrong
//! repo, a branch that doesn't match the execution — which is a normal
//! policy judgment, not an engine fault. [`ApplyDecision::Rejected`] carries
//! that refusal's reason back to [`WorkDb::submit_worker_proposal`], which
//! stores it as `state = 'rejected'` / `decision_reason`, distinct from an
//! `Err` (a genuine storage failure).
//!
//! `deferred_scope`'s second effect (the durable audit line appended to the
//! work item's description) is the one exception to "everything happens in
//! the transaction": appending it requires [`WorkDb::update_work_item`],
//! which dispatches through product/project/task-specific update paths
//! (event publication, Boothby capture, …) far too heavy to fold into this
//! transaction, and doing so would try to re-lock `WorkDb`'s single shared
//! connection from inside the transaction that already holds it — a
//! guaranteed deadlock (see [`WorkDb::connect`]'s own docs). So that line is
//! appended best-effort just after the transaction commits, mirroring
//! `record_deferred_scope_item`'s existing precedent of treating the audit
//! line as non-fatal.

use rusqlite::Transaction;

use super::automations::query_automation;
use super::*;
use crate::deferred_scope::DEFERRED_SCOPE_MARKER;
use crate::worker_escalation::{WORKER_BLOCKED_ATTENTION_KIND, WORKER_ESCALATION_ATTENTION_KIND};
use boss_protocol::{
    Attention, AttentionGroup, AttentionProposalPayload, AutomationOutcomeProposalPayload, BlockedProposalPayload,
    CreateAttentionInput, CreateAttentionItemInput, DeferredScopeProposalPayload, EffortEscalationProposalPayload,
    FollowupTaskProposalPayload, PrCreatedProposalPayload, ProposalKind,
};

/// `work_attention_items.kind` for an `attention` proposal that did not
/// specify its own `attention_kind`.
pub const ATTENTION_PROPOSAL_DEFAULT_KIND: &str = "worker_attention";

/// Whether [`apply_in_transaction`] runs synchronously at submission
/// (`AutoApply`) or the proposal stays `proposed` for a later/human
/// disposition path (`Gated`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalApplyPolicy {
    AutoApply,
    Gated,
}

/// The policy table. Exhaustive match: adding a `ProposalKind` variant is a
/// compile error here until this function says what its default disposition
/// is, mirroring the exhaustive-match discipline `ProposalKind` itself
/// documents (design §"Data model": "adding a kind is an engine change").
pub fn apply_policy(kind: ProposalKind) -> ProposalApplyPolicy {
    match kind {
        ProposalKind::Attention
        | ProposalKind::EffortEscalation
        | ProposalKind::Blocked
        | ProposalKind::DeferredScope
        | ProposalKind::AutomationOutcome
        | ProposalKind::PrCreated => ProposalApplyPolicy::AutoApply,
        // Gated by design: task creation from a followup proposal always
        // requires the human batch-accept gesture
        // (`WorkDb::action_attention_group`). See
        // `stage_followup_task_in_transaction` for what *does* still happen
        // synchronously at submission for this kind despite being gated.
        ProposalKind::FollowupTask => ProposalApplyPolicy::Gated,
    }
}

/// What a successful (accepted) [`apply_in_transaction`] call produced.
#[derive(Debug)]
pub struct ApplyOutcome {
    /// Id of the row the apply produced — stamped onto the proposal's
    /// `applied_ref` column in the same transaction. `None` for an outcome
    /// that legitimately produces no row (`automation_outcome`'s `skip`
    /// arm: there is nothing to point `applied_ref` at).
    pub applied_ref: Option<String>,
    /// `deferred_scope` only: the durable audit line to append to the work
    /// item's description once the transaction has committed (see the
    /// module doc for why this can't happen inside the transaction itself).
    pub post_commit_audit_line: Option<String>,
}

/// What [`apply_in_transaction`] decided for an `AutoApply` kind: either it
/// produced an effect ([`ApplyOutcome`]), or a policy check refused a
/// syntactically-valid payload — a provenance mismatch, a wrong repo, a
/// non-matching branch. Distinct from `Err`: a rejection is an expected,
/// typed disposition the worker is meant to see via `boss propose --list`,
/// not an engine fault.
#[derive(Debug)]
pub enum ApplyDecision {
    Applied(ApplyOutcome),
    /// Human-readable reason, stored verbatim as the proposal's
    /// `decision_reason`.
    Rejected(String),
}

/// Auto-apply `kind` inside `tx` — the same transaction
/// [`WorkDb::submit_worker_proposal`] inserts the `worker_proposals` row in.
/// Only called for kinds whose [`apply_policy`] is
/// [`ProposalApplyPolicy::AutoApply`]; `payload_json` is the already-
/// validated, canonically-serialised payload stored on the row. `proposal_id`
/// is the id already allocated for the row this call's outcome will be
/// stamped onto (allocated by the caller before this runs, so appliers that
/// need to reference their own proposal — e.g. to supersede a predecessor —
/// have it available before the `INSERT`).
pub fn apply_in_transaction(
    tx: &Transaction<'_>,
    execution_id: &str,
    payload_json: &str,
    kind: ProposalKind,
    proposal_id: &str,
) -> Result<ApplyDecision> {
    match kind {
        ProposalKind::Attention => apply_attention(tx, execution_id, payload_json).map(ApplyDecision::Applied),
        ProposalKind::EffortEscalation => {
            apply_effort_escalation(tx, execution_id, payload_json).map(ApplyDecision::Applied)
        }
        ProposalKind::Blocked => apply_blocked(tx, execution_id, payload_json).map(ApplyDecision::Applied),
        ProposalKind::DeferredScope => apply_deferred_scope(tx, execution_id, payload_json).map(ApplyDecision::Applied),
        ProposalKind::AutomationOutcome => apply_automation_outcome(tx, execution_id, payload_json, proposal_id),
        ProposalKind::PrCreated => apply_pr_created(tx, execution_id, payload_json),
        ProposalKind::FollowupTask => {
            anyhow::bail!(
                "no applier for `{kind}`; it is Gated by design and never routes through \
                 apply_in_transaction — see stage_followup_task_in_transaction"
            )
        }
    }
}

fn create_attention_item_row(
    tx: &Transaction<'_>,
    execution_id: &str,
    kind: &str,
    title: &str,
    body_markdown: String,
) -> Result<ApplyOutcome> {
    let input = CreateAttentionItemInput::builder()
        .execution_id(execution_id.to_owned())
        .kind(kind.to_owned())
        .title(title.to_owned())
        .body_markdown(body_markdown)
        .build();
    let item = crate::work::workitems::insert_attention_item_row(tx, &input)?;
    Ok(ApplyOutcome {
        applied_ref: Some(item.id),
        post_commit_audit_line: None,
    })
}

/// `attention` proposals auto-apply straight to a `work_attention_items`
/// row — the same table generic detector-filed attentions land in.
fn apply_attention(tx: &Transaction<'_>, execution_id: &str, payload_json: &str) -> Result<ApplyOutcome> {
    let payload: AttentionProposalPayload =
        serde_json::from_str(payload_json).context("attention proposal payload_json did not deserialize")?;
    let kind = payload
        .attention_kind
        .unwrap_or_else(|| ATTENTION_PROPOSAL_DEFAULT_KIND.to_owned());
    create_attention_item_row(tx, execution_id, &kind, &payload.title, payload.body_markdown)
}

/// `effort_escalation` proposals auto-apply the same as the legacy
/// `[effort-escalation]` marker path
/// ([`crate::completion::WorkerCompletionHandler::file_worker_signal_attention`]):
/// a `worker_escalation`-kind attention item. Filing that (unresolved) row
/// is the entire "auto-nudge pause" mechanism —
/// [`crate::completion::WorkerCompletionHandler::unresolved_worker_signal_reason`]
/// re-derives the pause reactively from `work_attention_items`, so there is
/// no separate pause flag to set here.
fn apply_effort_escalation(tx: &Transaction<'_>, execution_id: &str, payload_json: &str) -> Result<ApplyOutcome> {
    let payload: EffortEscalationProposalPayload =
        serde_json::from_str(payload_json).context("effort_escalation proposal payload_json did not deserialize")?;
    let body = format!(
        "Worker proposed an effort escalation to `{level}`.\n\n\
         - execution: `{execution_id}`\n\n\
         Reason:\n\n{reason}\n\n\
         The auto-nudge \"produce a PR\" loop is paused for this execution while this item is \
         unresolved. Acking the worker (e.g. `bossctl probe`) resolves it and resumes normal \
         nudging.",
        level = payload.requested_level,
        reason = payload.reason,
    );
    create_attention_item_row(
        tx,
        execution_id,
        WORKER_ESCALATION_ATTENTION_KIND,
        "Worker requested an effort escalation",
        body,
    )
}

/// `blocked` proposals auto-apply the same as the legacy `[blocked]` marker
/// path: a `worker_blocked`-kind attention item, pausing the auto-nudge loop
/// the same reactive way `effort_escalation` does.
fn apply_blocked(tx: &Transaction<'_>, execution_id: &str, payload_json: &str) -> Result<ApplyOutcome> {
    let payload: BlockedProposalPayload =
        serde_json::from_str(payload_json).context("blocked proposal payload_json did not deserialize")?;
    let body = format!(
        "Worker reported a blocker.\n\n\
         - execution: `{execution_id}`\n\n\
         Reason:\n\n{reason}\n\n\
         The auto-nudge \"produce a PR\" loop is paused for this execution while this item is \
         unresolved. Acking the worker (e.g. `bossctl probe`) resolves it and resumes normal \
         nudging.",
        reason = payload.reason,
    );
    create_attention_item_row(
        tx,
        execution_id,
        WORKER_BLOCKED_ATTENTION_KIND,
        "Worker reported a blocker",
        body,
    )
}

/// `deferred_scope` proposals auto-apply the same two effects
/// [`crate::completion::WorkerCompletionHandler::record_deferred_scope_item`]
/// writes for the legacy `[deferred-scope]` marker: a `deferred_scope`-kind
/// attention item (in-transaction, atomic with the proposal row) carrying
/// the verbatim `[deferred-scope] summary="…" reason="…"` marker line —
/// exactly what [`crate::deferred_scope::detect_deferred_scope_items`]
/// scans `body_markdown` for — plus a durable
/// `[deferred-scope] epoch …: summary="…" reason="…"` audit line appended to
/// the work item's description (best-effort, after commit — see the module
/// doc). Embedding the real marker line (rather than free prose) is what
/// lets [`crate::work::workitems::WorkDb::create_task_from_deferred_scope_attention`]
/// recover the real summary/reason, keeps the macOS
/// `DeferredScopeAttentionPresentation.parseMarker` badge readable, and
/// restores the legacy detectors' content-keyed dedup
/// (`body_markdown.contains(&marker_line)`) across the marker/proposal
/// transition period.
///
/// `payload.summary`/`payload.reason` cannot contain `"` or a newline —
/// `boss_engine_proposal_validation::validate_payload` rejects those for
/// `deferred_scope` precisely because they would corrupt this quoted,
/// single-line marker — so building the marker line here by direct
/// interpolation is safe.
fn apply_deferred_scope(tx: &Transaction<'_>, execution_id: &str, payload_json: &str) -> Result<ApplyOutcome> {
    let payload: DeferredScopeProposalPayload =
        serde_json::from_str(payload_json).context("deferred_scope proposal payload_json did not deserialize")?;
    let marker_line = format!(
        "{DEFERRED_SCOPE_MARKER} summary=\"{}\" reason=\"{}\"",
        payload.summary, payload.reason,
    );
    let body = format!(
        "Worker deferred part of this task's scope.\n\n\
         - execution: `{execution_id}`\n\n\
         Marker (verbatim):\n\n```\n{marker_line}\n```\n\n\
         This is NOT yet tracked work — the worker has no ability to file a task itself. Decide \
         whether to create a followup task for the deferred item, or consciously accept the \
         deferral (e.g. it is genuinely out of scope for this task). Either way, resolving this \
         item records that a human made that call, rather than the remainder silently vanishing.",
    );
    let mut outcome = create_attention_item_row(
        tx,
        execution_id,
        crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND,
        "Worker deferred scope",
        body,
    )?;

    let item = crate::deferred_scope::DeferredScopeItem {
        marker_line,
        parse_warning: None,
    };
    let epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    outcome.post_commit_audit_line = Some(crate::deferred_scope::render_audit_line(epoch, &item));
    Ok(outcome)
}

/// `followup_task` is [`ProposalApplyPolicy::Gated`] — task creation always
/// waits for the human batch-accept gesture — but the *group membership*
/// itself is not gated: the design requires the member to be visible in the
/// originating task's `followup` attention group "from the moment of
/// submission" (§"UI visibility and provenance": "No gated kind is invisible
/// while pending"). So this runs unconditionally at submission, regardless
/// of `apply_policy`, called directly by [`WorkDb::submit_worker_proposal`]
/// rather than through [`apply_in_transaction`]'s `AutoApply`-only dispatch.
///
/// Reuses [`crate::work::attentions::create_attention_in_tx`], which shares
/// only the `(grouping_key, generation)` group-resolution logic
/// [`crate::work::attentions::reconcile_attentions`] uses for the legacy
/// `FOLLOWUPS:` sentinel — so a proposal-submitted followup and a
/// detector-submitted one land in the same group, keyed on the originating
/// task. Unlike `reconcile_attentions`, `create_attention_in_tx` does not
/// dedup by content: a bare create is a one-shot, not content-idempotent
/// (see `WorkDb::create_attention`'s own docs). A second submission of
/// substantively the same followup (a different idempotency key, or an
/// explicit `--idempotency-key`) therefore appends a near-duplicate member;
/// the accept gesture's dedup/scoring verdicts are the quality gate for
/// that, not this staging step. `source_proposal_id` is stamped on the
/// member for provenance, mirroring the existing `confidence_source`
/// field's pattern.
///
/// Returns the member plus its group; the caller's `worker_proposals` row
/// stays `state = 'proposed'` with `applied_ref = None` regardless — nothing
/// is "applied" yet, since the member is not a task.
pub fn stage_followup_task_in_transaction(
    tx: &Transaction<'_>,
    work_item_id: &str,
    proposal_id: &str,
    payload_json: &str,
) -> Result<(Attention, AttentionGroup)> {
    let payload: FollowupTaskProposalPayload =
        serde_json::from_str(payload_json).context("followup_task proposal payload_json did not deserialize")?;
    let input = CreateAttentionInput::builder()
        .kind("followup")
        .association_task_id(work_item_id.to_owned())
        .source_task_id(work_item_id.to_owned())
        .proposed_name(payload.proposed_name)
        .proposed_description(payload.proposed_description)
        .maybe_proposed_effort(payload.proposed_effort.map(|e| e.as_str().to_owned()))
        .maybe_proposed_work_kind(payload.proposed_work_kind)
        .rationale(payload.rationale)
        .confidence_source("structured")
        .source_proposal_id(proposal_id.to_owned())
        .build();
    create_attention_in_tx(tx, input)
}

/// `automation_outcome` auto-applies with a provenance check mirroring the
/// legacy marker path's ([`crate::completion::WorkerCompletionHandler::finalize_automation_triage`],
/// `completion.rs:2414`): a `produced_task` outcome is only accepted when
/// the named task exists and carries a matching `source_automation_id` — the
/// same check that stops a triage worker from claiming credit for an
/// unrelated task. For an `automation_triage` execution the automation id
/// *is* `execution.work_item_id` (triage executions run against the
/// automation as their "work item", not a task).
///
/// A `skip` outcome carries no task to check and always applies.
///
/// Once the new payload is decided `Applied` (either arm — `Skip` or
/// `ProducedTask`), any of this execution's prior `automation_outcome`
/// proposals still in `proposed`/`applied` are marked `superseded`: a worker
/// only gets one operative triage outcome at a time, and a second accepted
/// submission (the worker revised its decision) means the first is no
/// longer the answer — design §"Data model": "`superseded` covers a newer
/// proposal with the same idempotency scope replacing an undecided one
/// (e.g. triage revises its outcome)". A *rejected* new payload replaces
/// nothing — the prior `Applied` outcome is still the operative one, so
/// superseding it would leave the execution with zero applied proposals.
///
/// This applier only decides and stamps the *proposal* row
/// (`applied`/`rejected` + `applied_ref` = the matched task id). Reading this
/// disposition to actually finalize triage
/// (`finalize_automation_triage_run` / `complete_pane_parked_execution`) is
/// implementation task 11's seam migration, not this one — task 6 only
/// builds the applier.
fn apply_automation_outcome(
    tx: &Transaction<'_>,
    execution_id: &str,
    payload_json: &str,
    proposal_id: &str,
) -> Result<ApplyDecision> {
    let payload: AutomationOutcomeProposalPayload =
        serde_json::from_str(payload_json).context("automation_outcome proposal payload_json did not deserialize")?;

    let decision = match payload {
        AutomationOutcomeProposalPayload::Skip { .. } => ApplyDecision::Applied(ApplyOutcome {
            applied_ref: None,
            post_commit_audit_line: None,
        }),
        AutomationOutcomeProposalPayload::ProducedTask { task_id } => {
            let execution = query_execution(tx, execution_id).require("execution", execution_id)?;
            let automation_id = execution.work_item_id;

            if query_automation(tx, &automation_id)?.is_none() {
                return Ok(ApplyDecision::Rejected(format!(
                    "this execution's work item {automation_id} is not an automation; \
                     automation_outcome can only be submitted by an automation_triage execution"
                )));
            }

            // `query_task`'s column list omits `source_automation_id` (it is
            // not part of the standard task-read shape most callers need),
            // so the provenance check reads it directly rather than through
            // the shared mapper — mirrors `WorkDb::source_automation_id_for_work_item`'s
            // query, tx-scoped.
            let task_source_automation_id: Option<Option<String>> = tx
                .query_row(
                    "SELECT source_automation_id FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                    [&task_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?;

            match task_source_automation_id {
                Some(Some(actual)) if actual == automation_id => ApplyDecision::Applied(ApplyOutcome {
                    applied_ref: Some(task_id),
                    post_commit_audit_line: None,
                }),
                Some(_) => ApplyDecision::Rejected(format!(
                    "task {task_id} exists but its source_automation_id does not match automation \
                     {automation_id}; only a task this automation itself produced can be claimed as its outcome"
                )),
                None => ApplyDecision::Rejected(format!(
                    "no task {task_id} exists (or it is soft-deleted); `produced_task` must name a task \
                     that already exists (e.g. created via `boss task create --automation`)"
                )),
            }
        }
    };

    if matches!(decision, ApplyDecision::Applied(_)) {
        supersede_prior_automation_outcomes(tx, execution_id, proposal_id)?;
    }

    Ok(decision)
}

/// Mark this execution's prior undecided `automation_outcome` proposals
/// `superseded`, per [`apply_automation_outcome`]'s doc comment. `proposal_id`
/// (the row about to be decided) is excluded, but at the point this runs it
/// has not been inserted yet, so the exclusion is defensive rather than
/// load-bearing.
fn supersede_prior_automation_outcomes(tx: &Transaction<'_>, execution_id: &str, proposal_id: &str) -> Result<()> {
    let now = now_string();
    tx.execute(
        "UPDATE worker_proposals
         SET state = 'superseded', decided_by = 'policy', decided_at = ?2,
             decision_reason = 'superseded by a newer automation_outcome proposal (' || ?3 || ') for the same execution'
         WHERE execution_id = ?1 AND kind = 'automation_outcome' AND state IN ('proposed', 'applied') AND id <> ?3",
        params![execution_id, now, proposal_id],
    )?;
    Ok(())
}

/// `pr_created` auto-applies with verification mirroring the legacy
/// hook-observed path: URL shape + product-repo slug
/// ([`crate::pr_url_capture::validate_pr_url`], `pr_url_capture.rs:86`) and,
/// when the worker supplied a `branch`, a branch-name match against the
/// execution ([`crate::completion::expected_branch_name`] /
/// [`crate::completion::branches_identify_same_work_item`] — the same
/// prefix-agnostic suffix match `verified_staged_pr_url` uses,
/// `completion.rs:1248`).
///
/// Unlike `verified_staged_pr_url`, this cannot make the async GitHub API
/// call that verifies a PR's actual head ref — appliers run synchronously
/// inside a DB transaction. The branch check here is therefore a pure,
/// synchronous string comparison against the worker-supplied `branch` field;
/// when the worker omits it, only the URL/repo check runs. The full
/// network-verified check stays at Stop-time finalization (implementation
/// task 12), which reads this proposal row instead of the in-memory
/// `StagedPrUrlCache`.
///
/// On success, binds the PR to the work item by opportunistically attaching
/// `pr_url` to the task — mirroring [`WorkDb::reconciler_attach_pr_url`]'s
/// "set if not already set" semantics rather than
/// [`WorkDb::record_worker_pr_completion`]'s full completion cascade (status
/// transition, execution finalization, run-summary capture): that heavier
/// lifting belongs to Stop-time finalization (task 12), not to a proposal
/// applier that only knows "a worker declared a PR" and has not yet driven
/// the execution to completion.
///
/// An execution whose work item is not a task (e.g. an `automation_triage`
/// execution, whose `work_item_id` is the automation id) is a typed
/// [`ApplyDecision::Rejected`], not an `Err` — a worker misusing `pr_created`
/// from the wrong execution kind is a normal policy refusal the caller is
/// meant to see via `boss propose --list`, not an engine fault that aborts
/// the whole submission (leaving no durable proposal row at all).
fn apply_pr_created(tx: &Transaction<'_>, execution_id: &str, payload_json: &str) -> Result<ApplyDecision> {
    let payload: PrCreatedProposalPayload =
        serde_json::from_str(payload_json).context("pr_created proposal payload_json did not deserialize")?;

    let execution = query_execution(tx, execution_id).require("execution", execution_id)?;
    let Some(task) = query_task(tx, &execution.work_item_id)? else {
        return Ok(ApplyDecision::Rejected(format!(
            "this execution's work item {} is not a task; pr_created can only bind a PR to a task/chore",
            execution.work_item_id
        )));
    };
    let product = query_product(tx, &task.product_id).require("product", &task.product_id)?;

    let Some(repo_remote_url) = product.repo_remote_url.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(ApplyDecision::Rejected(format!(
            "product {} has no repo_remote_url configured; cannot verify a PR URL against it",
            product.id
        )));
    };

    if let Err(reason) = crate::pr_url_capture::validate_pr_url(&payload.pr_url, repo_remote_url) {
        return Ok(ApplyDecision::Rejected(reason));
    }

    if let Some(branch) = payload.branch.as_deref() {
        let expected = crate::completion::expected_branch_name(
            execution_id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
        );
        if !crate::completion::branches_identify_same_work_item(branch, &expected) {
            return Ok(ApplyDecision::Rejected(format!(
                "branch `{branch}` does not identify this execution's work item (expected a branch \
                 matching `{expected}`)"
            )));
        }
    }

    if let Some(existing) = task.pr_url.as_deref().filter(|s| !s.is_empty())
        && existing != payload.pr_url
    {
        tracing::warn!(
            execution_id,
            task_id = %task.id,
            existing_pr_url = existing,
            declared_pr_url = %payload.pr_url,
            "pr_created proposal declared a different PR than the task already has bound; \
             keeping the existing binding",
        );
    }

    tx.execute(
        "UPDATE tasks SET pr_url = ?2, updated_at = ?3 \
         WHERE id = ?1 AND deleted_at IS NULL AND (pr_url IS NULL OR pr_url = '')",
        params![task.id, payload.pr_url, now_string()],
    )?;

    Ok(ApplyDecision::Applied(ApplyOutcome {
        applied_ref: Some(task.id),
        post_commit_audit_line: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::ProposalState;

    #[test]
    fn a_rejected_automation_outcome_does_not_supersede_a_valid_predecessor() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let chore = crate::test_support::create_test_chore(&db, product.id, "Triage");
        let execution = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());

        let skip = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: &chore.id,
                kind: ProposalKind::AutomationOutcome,
                payload_json: r#"{"outcome":"skip","reason":"repo is clean"}"#,
                idempotency_key: "skip-1",
            })
            .unwrap()
            .unwrap();
        assert_eq!(skip.proposal.state, ProposalState::Applied);

        let rejected = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: &chore.id,
                kind: ProposalKind::AutomationOutcome,
                payload_json: r#"{"outcome":"produced_task","task_id":"task_nonexistent"}"#,
                idempotency_key: "produced-1",
            })
            .unwrap()
            .unwrap();
        assert_eq!(rejected.proposal.state, ProposalState::Rejected);

        let refetched_skip = db
            .list_worker_proposals_for_work_item(&chore.id, None, None)
            .unwrap()
            .into_iter()
            .find(|p| p.id == skip.proposal.id)
            .expect("the original skip proposal must still exist");
        assert_eq!(
            refetched_skip.state,
            ProposalState::Applied,
            "a rejected replacement must not supersede the still-valid predecessor"
        );
    }

    /// `pr_created` submitted from an execution whose work item is not a
    /// task (e.g. an `automation_triage` execution) must be a typed
    /// `Rejected` disposition — a durable proposal row the worker can see
    /// via `boss propose --list` — not an engine `Err` that aborts the
    /// whole submission with no row written at all.
    #[test]
    fn pr_created_from_a_non_task_execution_is_rejected_not_erred() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let automation = crate::test_support::seed_daily_automation(&db, &product.id);
        let execution = db
            .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
            .unwrap();

        let outcome = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: &automation.id,
                kind: ProposalKind::PrCreated,
                payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/1"}"#,
                idempotency_key: "pr-1",
            })
            .unwrap()
            .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Rejected);
        assert!(
            outcome
                .proposal
                .decision_reason
                .as_deref()
                .unwrap_or_default()
                .contains("is not a task"),
            "decision_reason should explain the work item is not a task: {:?}",
            outcome.proposal.decision_reason
        );
    }

    /// `automation_outcome` submitted from an execution whose work item is
    /// not actually an automation (a normal task/chore execution) must
    /// reject with a message naming the real problem, not the confusing
    /// "source_automation_id does not match" provenance-mismatch text.
    #[test]
    fn automation_outcome_from_a_non_automation_execution_names_the_real_problem() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let chore = crate::test_support::create_test_chore(&db, product.id, "Not triage");
        let execution = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());

        let outcome = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: &chore.id,
                kind: ProposalKind::AutomationOutcome,
                payload_json: r#"{"outcome":"produced_task","task_id":"task_whatever"}"#,
                idempotency_key: "auto-outcome-1",
            })
            .unwrap()
            .unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Rejected);
        assert!(
            outcome
                .proposal
                .decision_reason
                .as_deref()
                .unwrap_or_default()
                .contains("is not an automation"),
            "decision_reason should name the real problem, not a provenance mismatch: {:?}",
            outcome.proposal.decision_reason
        );
    }

    /// Binding a PR via `pr_created` must stamp `updated_at`, mirroring
    /// `WorkDb::reconciler_attach_pr_url`'s "set if not already set"
    /// semantics — downstream sorting and drift reconcilers key off it.
    #[test]
    fn pr_created_binding_updates_updated_at() {
        let (_dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let chore = crate::test_support::create_test_chore(&db, product.id, "Ship it");
        let execution = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());
        // Back-date so `now_string()`'s second-resolution stamp is guaranteed
        // to differ from `before`, without burning wall-clock on a sleep.
        let before = "2020-01-01T00:00:00Z".to_owned();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
                params![chore.id, before],
            )
            .unwrap();

        let outcome = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &execution.id,
                work_item_id: &chore.id,
                kind: ProposalKind::PrCreated,
                payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/42"}"#,
                idempotency_key: "pr-42",
            })
            .unwrap()
            .unwrap();
        assert_eq!(outcome.proposal.state, ProposalState::Applied);

        let after = query_task(&db.connect().unwrap(), &chore.id).unwrap().unwrap();
        assert_eq!(
            after.pr_url.as_deref(),
            Some("https://github.com/spinyfin/mono/pull/42")
        );
        assert_ne!(after.updated_at, before, "binding a PR must stamp updated_at");
    }

    #[test]
    fn auto_apply_kinds_match_the_design_table() {
        assert_eq!(apply_policy(ProposalKind::Attention), ProposalApplyPolicy::AutoApply);
        assert_eq!(
            apply_policy(ProposalKind::EffortEscalation),
            ProposalApplyPolicy::AutoApply
        );
        assert_eq!(apply_policy(ProposalKind::Blocked), ProposalApplyPolicy::AutoApply);
        assert_eq!(
            apply_policy(ProposalKind::DeferredScope),
            ProposalApplyPolicy::AutoApply
        );
        assert_eq!(
            apply_policy(ProposalKind::AutomationOutcome),
            ProposalApplyPolicy::AutoApply
        );
        assert_eq!(apply_policy(ProposalKind::PrCreated), ProposalApplyPolicy::AutoApply);
    }

    /// If a `Gated` kind is ever flipped to `AutoApply` in [`apply_policy`]
    /// before its arm is added to [`apply_in_transaction`]'s match, the
    /// submission must fail with a typed error the caller sees — not panic
    /// and take the socket connection down with it.
    #[test]
    fn flipping_a_gated_kind_to_autoapply_without_an_applier_errors_instead_of_panicking() {
        let (_dir, db) = crate::test_support::open_db();
        let mut conn = db.connect().unwrap();
        let tx = conn.transaction().unwrap();

        let err = apply_in_transaction(&tx, "exec_missing", "{}", ProposalKind::FollowupTask, "prp_missing")
            .expect_err("no applier exists for FollowupTask; this must be an error, not a panic");
        assert!(
            err.to_string().contains("followup_task"),
            "error should name the unhandled kind: {err}"
        );
    }

    /// `followup_task` is the only kind implementation task 6 keeps gated —
    /// `automation_outcome` and `pr_created` both flip to `AutoApply` in this
    /// PR (real appliers above), leaving `followup_task` as the sole
    /// permanently-gated kind (per design: task creation always needs the
    /// human batch-accept gesture).
    #[test]
    fn followup_task_stays_gated() {
        assert_eq!(apply_policy(ProposalKind::FollowupTask), ProposalApplyPolicy::Gated);
    }
}
