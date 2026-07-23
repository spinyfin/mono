//! Apply pipeline for auto-apply worker-proposal kinds.
//!
//! Implementation task 5 of `worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! ("Apply pipeline: auto-apply kinds"): a policy table plus synchronous
//! appliers for `attention`, `effort_escalation`, `blocked`, and
//! `deferred_scope` — each writing the same `work_attention_items` rows
//! today's marker detectors write ([`crate::completion::WorkerCompletionHandler::file_worker_signal_attention`],
//! [`crate::completion::WorkerCompletionHandler::record_deferred_scope_item`]),
//! so a submitted proposal and its effect are the same event.
//!
//! [`apply_policy`] is a plain code table, not a DB row or feature flag: per
//! the design's risk note ("If auto-applied attention proposals get noisy,
//! the policy table can flip that kind to gated without schema change"),
//! flipping a kind's disposition is a one-line code change here, not a
//! migration. `followup_task` / `automation_outcome` / `pr_created` are
//! marked [`ProposalApplyPolicy::Gated`] in this PR — `followup_task` is
//! gated by design (the human batch-accept gesture); the other two
//! auto-apply per the design too, but their appliers land in implementation
//! task 6, which extends this same module.
//!
//! [`apply_in_transaction`] runs inside [`WorkDb::submit_worker_proposal`]'s
//! existing `Immediate` transaction, so the produced `work_attention_items`
//! row commits or rolls back together with the `worker_proposals` row it is
//! `applied_ref`-linked from — "proposal accepted" and "effect exists" are
//! the same commit. `deferred_scope`'s second effect (the durable audit
//! line appended to the work item's description) is the one exception:
//! appending it requires [`WorkDb::update_work_item`], which dispatches
//! through product/project/task-specific update paths (event publication,
//! Boothby capture, …) far too heavy to fold into this transaction, and
//! doing so would try to re-lock `WorkDb`'s single shared connection from
//! inside the transaction that already holds it — a guaranteed deadlock
//! (see [`WorkDb::connect`]'s own docs). So that line is appended
//! best-effort just after the transaction commits, mirroring
//! `record_deferred_scope_item`'s existing precedent of treating the audit
//! line as non-fatal.

use rusqlite::Transaction;

use super::*;
use crate::deferred_scope::DEFERRED_SCOPE_MARKER;
use crate::worker_escalation::{WORKER_BLOCKED_ATTENTION_KIND, WORKER_ESCALATION_ATTENTION_KIND};
use boss_protocol::{
    AttentionProposalPayload, BlockedProposalPayload, CreateAttentionItemInput, DeferredScopeProposalPayload,
    EffortEscalationProposalPayload, ProposalKind,
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
        | ProposalKind::DeferredScope => ProposalApplyPolicy::AutoApply,
        ProposalKind::FollowupTask | ProposalKind::AutomationOutcome | ProposalKind::PrCreated => {
            ProposalApplyPolicy::Gated
        }
    }
}

/// What a successful [`apply_in_transaction`] call produced.
pub struct ApplyOutcome {
    /// Id of the row the apply produced — stamped onto the proposal's
    /// `applied_ref` column in the same transaction.
    pub applied_ref: String,
    /// `deferred_scope` only: the durable audit line to append to the work
    /// item's description once the transaction has committed (see the
    /// module doc for why this can't happen inside the transaction itself).
    pub post_commit_audit_line: Option<String>,
}

/// Auto-apply `kind` inside `tx` — the same transaction
/// [`WorkDb::submit_worker_proposal`] inserts the `worker_proposals` row in.
/// Only called for kinds whose [`apply_policy`] is
/// [`ProposalApplyPolicy::AutoApply`]; `payload_json` is the already-
/// validated, canonically-serialised payload stored on the row.
pub fn apply_in_transaction(
    tx: &Transaction<'_>,
    execution_id: &str,
    payload_json: &str,
    kind: ProposalKind,
) -> Result<ApplyOutcome> {
    match kind {
        ProposalKind::Attention => apply_attention(tx, execution_id, payload_json),
        ProposalKind::EffortEscalation => apply_effort_escalation(tx, execution_id, payload_json),
        ProposalKind::Blocked => apply_blocked(tx, execution_id, payload_json),
        ProposalKind::DeferredScope => apply_deferred_scope(tx, execution_id, payload_json),
        ProposalKind::FollowupTask | ProposalKind::AutomationOutcome | ProposalKind::PrCreated => {
            unreachable!("apply_in_transaction is only called for AutoApply kinds; {kind} is Gated")
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
        applied_ref: item.id,
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
/// attention item (in-transaction, atomic with the proposal row) plus a
/// durable `[deferred-scope] epoch …: summary="…" reason="…"` audit line
/// appended to the work item's description (best-effort, after commit —
/// see the module doc).
fn apply_deferred_scope(tx: &Transaction<'_>, execution_id: &str, payload_json: &str) -> Result<ApplyOutcome> {
    let payload: DeferredScopeProposalPayload =
        serde_json::from_str(payload_json).context("deferred_scope proposal payload_json did not deserialize")?;
    let body = format!(
        "Worker deferred part of this task's scope.\n\n\
         - execution: `{execution_id}`\n\n\
         Summary of what was deferred:\n\n{summary}\n\n\
         Reason:\n\n{reason}\n\n\
         This is NOT yet tracked work — the worker has no ability to file a task itself. Decide \
         whether to create a followup task for the deferred item, or consciously accept the \
         deferral (e.g. it is genuinely out of scope for this task). Either way, resolving this \
         item records that a human made that call, rather than the remainder silently vanishing.",
        summary = payload.summary,
        reason = payload.reason,
    );
    let mut outcome = create_attention_item_row(
        tx,
        execution_id,
        crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND,
        "Worker deferred scope",
        body,
    )?;

    let epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    outcome.post_commit_audit_line = Some(format!(
        "\n{DEFERRED_SCOPE_MARKER} epoch {epoch}: summary=\"{}\" reason=\"{}\"",
        payload.summary, payload.reason,
    ));
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn gated_kinds_stay_gated_pending_task_6() {
        assert_eq!(apply_policy(ProposalKind::FollowupTask), ProposalApplyPolicy::Gated);
        assert_eq!(
            apply_policy(ProposalKind::AutomationOutcome),
            ProposalApplyPolicy::Gated
        );
        assert_eq!(apply_policy(ProposalKind::PrCreated), ProposalApplyPolicy::Gated);
    }
}
