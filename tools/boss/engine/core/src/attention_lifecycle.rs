//! Declarative lifecycle for every operator-visible failure signal the
//! engine can raise: what raises it, and — the part that was missing —
//! what specifically lowers it again.
//!
//! ## Why this exists
//!
//! Attention items and the kanban card's dispatch-failure banner are both
//! *signals*: the engine raises them when something goes wrong so an
//! operator can see it. Raising was implemented once, centrally
//! ([`crate::work::WorkDb::upsert_work_item_attention`] and friends).
//! Resolution was not: each kind's producer was left to invent its own
//! clearing path, so whether a signal could ever come down depended
//! entirely on whether whoever added that kind happened to also write a
//! resolve call. A survey of the 40 kinds below found only six with any
//! automatic path at all — `churn_guard_parked`, `dispatch_stage_stalled`,
//! `chain_serialized_stall`, `abandoned_branch_no_pr`,
//! `pr_review_died_without_findings`, and the external-tracker fetch
//! family. Everything else could be raised but never lowered, which is how
//! a work item ends up in Merging with a bound PR, four completed revisions,
//! and seven still-`open` attentions describing failures that a later
//! successful pass had already superseded.
//!
//! The same defect shape hit `tasks.dispatch_failed_reason` /
//! `dispatch_failed_error` — the columns behind the card's red
//! "Failed to start — …" banner ([`WorkDispatchFailureBanner`] in the
//! macOS app). Those are stamped by
//! [`crate::work::WorkDb::bounce_dispatch_failed_to_backlog`] and were
//! cleared in exactly one place: `request_execution_in_tx_with_live_check`,
//! i.e. only when a *deliberate re-dispatch of that work item* went through
//! that specific function. Every other way an item can start moving again —
//! the reconciler minting an execution, a `pr_review` / `revision` /
//! `ci_remediation` execution dispatched by the review pipeline — left the
//! stamp in place forever.
//!
//! ## The rule
//!
//! A signal comes down when, and only when, there is positive evidence that
//! the condition it describes is over. Not on a timer, not on a lane change,
//! not on operator hand-dismissal. [`ClearedBy`] enumerates the four shapes
//! that evidence can take, and every kind below declares one — including
//! the kinds whose honest answer is "a human has to decide this", which are
//! marked [`ClearedBy::HumanDecision`] rather than left silently blank.
//!
//! The engine applies the automatic variants; see
//! [`crate::work::WorkDb::reconcile_stale_attention_signals`] for the query
//! and [`crate::attention_reconcile_sweep`] for the periodic pass. The app
//! is a thin client throughout: it renders whatever is `open` and whatever
//! the columns say, and gained no new state interpretation.
//!
//! ## Where the boundary is
//!
//! Every automatic rule requires the evidence to *postdate the signal*. A
//! run that started before the attention was filed proves nothing about it;
//! only a run that started at or after it does. That is what keeps a
//! genuinely-broken item loud: if dispatch keeps failing, nothing ever
//! starts, no evidence ever accrues, and the banner and the attentions stay
//! exactly where they are. Nothing is deleted either — resolution stamps
//! `status = 'resolved'` and `resolved_at`, so the history stays
//! inspectable via `bossctl` and the attention surface's resolved view.

use boss_protocol::ExecutionKind;

/// What lowers a raised signal.
///
/// The variants are ordered from "the engine can prove this itself" to "only
/// a human can". [`Self::WorkResumed`] and [`Self::ExecutionKindCompleted`]
/// are the two the generic reconciler acts on; the other two are declarations
/// that no generic rule applies, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearedBy {
    /// The signal asserts that the engine could not get work moving on this
    /// item — it failed to start, stalled before dispatch, lost its pane,
    /// or was parked by a guard. Any run for the same work item that starts
    /// at or after the signal was raised is a direct contradiction of that
    /// assertion, so the signal is resolved.
    ///
    /// Deliberately keyed on a *run start*, not on the work item's kanban
    /// status: a card dragged between lanes proves nothing about whether
    /// dispatch works, whereas a worker actually starting does.
    WorkResumed,
    /// The signal asserts that a particular kind of pass failed to produce
    /// its output for this item (a reviewer that died mid-run, a reviewer
    /// that gave up without a readable result). A later execution of that
    /// same kind reaching `completed` supersedes it — the output the signal
    /// said was missing now exists.
    ///
    /// Scoped to the kind on purpose: a churned `chore_implementation`
    /// retry must never mask a still-dead `pr_review`.
    ExecutionKindCompleted(ExecutionKind),
    /// The kind's own producer re-evaluates the live condition on every one
    /// of its passes and resolves the item itself when the condition clears
    /// (the external-tracker reconcile loop on a successful fetch, the
    /// abandoned-branch sweep when a PR appears, the coordinator's probe
    /// acking a worker signal). The generic reconciler stays out of the way:
    /// it has no view of the condition, and the producer's answer is better
    /// than any evidence proxy this module could invent.
    ProducerReconciles,
    /// The signal records something a later success does not undo — a cost
    /// envelope that was already overrun, a command that already ran
    /// unobserved, a PR that already merged without review, a gate the
    /// operator is meant to open — or it is an operator work queue rather
    /// than a failure at all. Auto-resolving these would destroy the record
    /// the item exists to keep.
    ///
    /// This is not "we didn't get around to it": each entry below carries
    /// the reason no automatic evidence is sufficient.
    HumanDecision,
}

impl ClearedBy {
    /// Whether [`crate::work::WorkDb::reconcile_stale_attention_signals`]
    /// acts on this variant. `false` means the kind is deliberately outside
    /// the generic reconciler (its producer owns resolution, or a human
    /// does).
    pub fn is_automatic(&self) -> bool {
        matches!(self, Self::WorkResumed | Self::ExecutionKindCompleted(_))
    }
}

/// One kind's declared lifecycle.
#[derive(Debug, Clone)]
pub struct AttentionLifecycle {
    /// The `work_attention_items.kind` value.
    pub kind: &'static str,
    /// What lowers it.
    pub cleared_by: ClearedBy,
    /// Why that rule is the right one for this kind — the justification a
    /// future reader needs in order to change it safely. Kept on the data
    /// rather than in a doc comment so it can be dumped alongside the kind.
    pub rationale: &'static str,
}

const fn entry(kind: &'static str, cleared_by: ClearedBy, rationale: &'static str) -> AttentionLifecycle {
    AttentionLifecycle {
        kind,
        cleared_by,
        rationale,
    }
}

/// Every `work_attention_items.kind` the engine writes, with its clearing
/// rule. Adding a kind without adding it here means the generic reconciler
/// cannot see it, so filing runs a guard:
/// `crate::work::warn_if_lifecycle_undeclared` logs a warning for an
/// unregistered kind and is called from *every* filing path — the
/// work-item upsert, the execution-scoped `create_attention_item` /
/// `insert_attention_item_row`, and the two bespoke raw-INSERT helpers.
/// `every_registered_kind_is_declared_once` below pins the table itself.
pub const ATTENTION_LIFECYCLES: &[AttentionLifecycle] = &[
    // ── Cleared by work resuming ────────────────────────────────────────
    entry(
        crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "The guard's whole claim is 'we stopped auto-redispatching this item'. A run starting is \
         the redispatch it said did not happen. `request_execution_in_tx_with_live_check` already \
         resolves it on the deliberate-retry path; this covers every other way the item starts.",
    ),
    entry(
        crate::work::DISPATCH_STAGE_STALLED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts an execution is wedged before dispatch. `dispatch_claimed_execution` resolves it \
         on slot claim; a run start is the same fact observed one step later, and covers the case \
         where the claim path resolved nothing because the engine restarted in between.",
    ),
    entry(
        crate::coordinator::CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the item is queued behind a chain sibling. The drain loop resolves it on bypass \
         and on unblock; a run start is unambiguous evidence the serialization ended.",
    ),
    entry(
        REPO_UNRESOLVED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the item has no repo resolution. `ensure_dispatch_repo_resolvable` is a hard gate \
         on every dispatch path, so an execution that reached a run start necessarily resolved a \
         repo — the condition cannot still hold.",
    ),
    entry(
        crate::dead_pid_sweep::PANE_DEATH_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Records that the item's worker pane died and was reconciled. A later run start is the \
         item working again; the dead pane is history, not current state.",
    ),
    entry(
        crate::remote_lease_reconcile::REMOTE_WORKER_DIED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Records that the item's worker died on a remote host and was reaped, and carries the worker's last \
         output so the cause is readable after the workspace goes back to cube. A later run start is the \
         redispatch that reap made room for; if the underlying host problem persists the next death files a \
         fresh item, so nothing is lost by clearing this one.",
    ),
    entry(
        crate::tmux_adoption::TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Records that boot-time adoption refused and reaped a version-skewed tmux session for \
         the item. A later run start is the redispatch that refusal deliberately made room for.",
    ),
    entry(
        crate::spawn_ack_sweep::DRIVER_START_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the worker's driver never started. A later run start is the direct contradiction.",
    ),
    entry(
        crate::app::readoption::PROGRESS_INGRESS_UNRECOVERABLE_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts progress for a specific run could not be ingested. A later run supersedes that \
         run entirely, and brings its own ingress.",
    ),
    entry(
        crate::completion::NUDGE_BREAKER_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the auto-nudge loop was bounded for a parked run. A later run start means a fresh \
         run with a fresh breaker; the old park is no longer the item's state.",
    ),
    entry(
        crate::completion::DRIVER_TERMINAL_ERROR_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the provider itself failed a run. A later run start is the provider working again \
         for this item. Distinct from `worker_recovery_*` below, which are dispatch *gates* whose \
         resolution is the operator's re-enable gesture, not an observation.",
    ),
    entry(
        crate::coordinator::PANE_SPAWN_FAILED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the worker pane for a run never came up. A later run starting for the same item is \
         direct evidence the pane-spawn problem is no longer blocking it — the same reasoning as \
         `driver_terminal_error`, one stage earlier in the run's lifecycle. The qualifier matters \
         most here: `work_runs.started_at` is stamped before the pane is asked for, so a redispatch \
         that fails to spawn identically must NOT count, and the evidence clause excludes runs whose \
         own execution raised this same kind.",
    ),
    entry(
        crate::coordinator::ANSWER_AGENT_READY_AGE_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Names a specific question that waited beyond the answer-agent queue-age threshold. A run start \
         for that question proves the queue wait ended; the scheduler also refreshes this signal while \
         the execution remains ready so an operator can distinguish an active wait from stale history.",
    ),
    // ── Cleared by a later completed pass of the same kind ──────────────
    entry(
        crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND,
        ClearedBy::ExecutionKindCompleted(ExecutionKind::PrReview),
        "Asserts a reviewer died before finalizing. `finalize_pr_review_pass` already resolves it \
         inline on every completed pass; this is the backstop for passes finalized while a prior \
         engine process was running, and the shape the rest of the table is modelled on.",
    ),
    // ── The producer owns resolution ────────────────────────────────────
    entry(
        crate::stale_worker_sweep::STALE_WORKER_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The stale-worker sweep rechecks tmux evidence every pass and resolves this attention when the \
         same worker resumes terminal activity or terminalizes.",
    ),
    entry(
        crate::worker_escalation::WORKER_ESCALATION_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The coordinator's probe IS the documented ack gesture; \
         `resolve_worker_signal_attentions_for_execution` fires there. An automatic clear would \
         un-pause the suppressed auto-nudge without the coordinator having answered.",
    ),
    entry(
        crate::worker_escalation::WORKER_BLOCKED_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "Same coordinator-ack contract as `worker_escalation`: the marker pauses this run's auto-nudge \
         loop until the coordinator answers, so only the probe may lower it.",
    ),
    entry(
        crate::abandoned_branch_pr_sweep::ATTENTION_KIND_ABANDONED_BRANCH_NO_PR,
        ClearedBy::ProducerReconciles,
        "The sweep re-checks the branch every pass and resolves the item itself once a PR appears \
         — a direct read of the condition, strictly better than any run-start proxy.",
    ),
    entry(
        EXTERNAL_TRACKER_AUTH_FAILED_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "Product-scoped, not work-item-scoped: the reconcile loop resolves it on the next \
         successful fetch. The generic reconciler's evidence is work-item runs, which say nothing \
         about a product's tracker credentials.",
    ),
    entry(
        EXTERNAL_TRACKER_TOKEN_REVOKED_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "Same product-scoped fetch-success path as `external_tracker_auth_failed`; the reconcile loop \
         resolves all three kinds together on the next clean fetch.",
    ),
    entry(
        EXTERNAL_TRACKER_TRANSIENT_ERRORS_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "Same product-scoped fetch-success path as `external_tracker_auth_failed`; a run of an \
         unrelated work item is not evidence the tracker stopped erroring.",
    ),
    // ── Only a human can lower these ────────────────────────────────────
    entry(
        crate::app::probes::PROBE_UNDELIVERED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a specific instruction never reached the worker. The run it targeted is over \
         by the time this is filed, so a later run of the same item cannot deliver that probe — it \
         is a different worker with a different prompt. Whether the instruction still needs to be \
         given, and through which channel, is the issuer's call.",
    ),
    entry(
        crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The deferred remainder still exists after any number of successful runs. Closing it is a \
         decision (spin up a followup, or accept the gap) with its own verb, \
         `resolve_deferred_scope_attention`.",
    ),
    entry(
        crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "It is a gate, not a report: the item is held in `blocked:deletion_signoff` until a human \
         signs off. Auto-resolving would release the hold the item exists to hold.",
    ),
    entry(
        crate::completion::REVIEW_RESULT_GIVEUP_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a PR advanced to Review with NO automated review — a fact a later pass does \
         not undo, same reasoning as `revision_no_changes_needed`. It cannot be \
         `ExecutionKindCompleted(PrReview)`: `finalize_pr_review_pass` files this item and then \
         falls straight through to `record_worker_pr_completion`, which stamps that very \
         `pr_review` execution `completed` in the same (or next) second — so the offending \
         execution would supply its own clearing evidence and the signal would self-resolve on the \
         first sweep, every time.",
    ),
    entry(
        crate::completion::REVISION_NO_OP_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a worker declined a reviewer's finding. Whether that judgement stands is the \
         human's call; later runs do not answer it.",
    ),
    entry(
        crate::completion::MID_TURN_REAP_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a revision implementation's worker was torn down while still mid-turn (activity=working) \
         instead of at its own Stop boundary — unpushed work or unrun post-push steps may be lost \
         (incident-004). The execution this happened to is already terminal by the time the item is \
         filed, so no later run of it can supply contradicting evidence; whether the lost work needs \
         redoing is the operator's call, same reasoning as `revision_no_changes_needed`.",
    ),
    entry(
        crate::merge_mechanism::PUSH_RESTRICTION_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "A repository permission the engine cannot change or observe changing. Nothing the engine \
         sees would constitute evidence.",
    ),
    entry(
        crate::envelope_watch::ENVELOPE_OVERRUN_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "A cost record. The overrun happened; a later cheap run does not un-spend it.",
    ),
    entry(
        crate::codex_unobserved_command::UNOBSERVED_COMMAND_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "An audit record of a command that already ran outside the observed surface. Auto-resolving \
         an audit trail on later success is exactly what an audit trail must not do.",
    ),
    entry(
        crate::codex_unobserved_command::UNOBSERVED_COMMAND_OVERFLOW_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Same audit-record reasoning as `codex_unobserved_command`, and the overflow variant additionally \
         records that further unobserved commands went uncounted — a gap no later run fills in.",
    ),
    entry(
        crate::work::ATTENTION_KIND_RECOVERY_PERMANENT,
        ClearedBy::HumanDecision,
        "This is a dispatch gate, not a report: `list_orphan_active_candidates` excludes items with \
         it open, and its own body tells the operator that resolving it re-enables redispatch. It \
         must stay open until the human says the underlying API problem is fixed.",
    ),
    entry(
        crate::work::ATTENTION_KIND_RECOVERY_EXHAUSTED,
        ClearedBy::HumanDecision,
        "Same dispatch-gate contract as `worker_recovery_permanent_error`: `list_orphan_active_candidates` \
         excludes items with it open, so resolving it is the operator re-enabling redispatch.",
    ),
    entry(
        crate::husk_pane_sweep::HUSK_BREAKER_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "About pane retirement, not about this work item making progress — a new run starting says \
         nothing about whether husk retirement still fails.",
    ),
    entry(
        crate::answer_agent_completion_sweep::ANSWER_AGENT_STRANDED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The doc comment the answer agent was dispatched for is still unanswered. An implementation \
         run starting on the same item does not answer it.",
    ),
    entry(
        crate::proposal_channel_error::PROPOSAL_CHANNEL_ERROR_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records a worker proposal the engine could not accept — the proposal's content is lost and \
         needs a human to re-file it. A later run does not recover it.",
    ),
    entry(
        crate::spawn_health::SPAWN_CAPABILITY_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Host-level spawn capability, not per-item state. One item starting elsewhere is not \
         evidence the unhealthy host recovered.",
    ),
    entry(
        crate::ci_watch::CI_REMEDIATION_EXHAUSTED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The attempt budget is spent. Clearing it on a later run would silently hand the item a \
         fresh budget it was never granted.",
    ),
    entry(
        crate::trunk_queue_poller::TRUNK_QUEUE_UNREACHABLE_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Queue-level infrastructure state. The poller does not currently resolve these, and a \
         work-item run start is not evidence about the queue — leaving them for the operator is \
         the honest answer until the poller grows a resolve path of its own.",
    ),
    entry(
        crate::trunk_queue_poller::TRUNK_TOKEN_REJECTED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "A rejected credential: nothing the engine can observe replaces a token, and a work item \
         starting a run says nothing about whether the Trunk token is valid again.",
    ),
    entry(
        crate::trunk_queue_poller::TRUNK_QUEUE_NOT_RUNNING_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Queue-level infrastructure state; see `trunk_queue_unreachable`. A stopped merge queue is \
         restarted by an operator, not by anything this engine can watch happen.",
    ),
    entry(
        crate::trunk_queue_poller::TRUNK_QUEUE_ENTRY_CANCELLED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a merge attempt was cancelled out of the queue — a decision to re-submit is \
         the operator's.",
    ),
    entry(
        crate::trunk_queue_poller::TRUNK_QUEUE_MERGE_FAILURE_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records a failed merge. The PR is still unmerged until someone acts.",
    ),
    entry(
        crate::trunk_queue_poller::TRUNK_QUEUE_RESUBMIT_STALLED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The PR is out of the merge queue with nothing left running that would put it back. \
         Re-submitting is a merge click, which is a human decision — and a later run of some \
         unrelated work item is not evidence that this PR re-entered the queue.",
    ),
    entry(
        EXTERNAL_TRACKER_REMOVED_UPSTREAM_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The upstream issue is gone; whether the Boss row should follow it is a human call.",
    ),
    entry(
        EXTERNAL_TRACKER_PERMISSION_DENIED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "An upstream permission a human must grant.",
    ),
    entry(
        REVISION_ARCHIVED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that the engine auto-archived a revision as moot — the point is that the operator \
         sees a state change they did not make. Auto-resolving it restores the silent-change \
         behaviour it was added to remove.",
    ),
    entry(
        FOLLOWUP_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "An operator work queue, not a failure signal: pending followup proposals wait for the \
         human batch-accept gesture and outlive their execution by design.",
    ),
    entry(
        QUESTION_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "A question posed to the operator, not a failure signal at all. It is lowered by being answered \
         — a later run does not supply the answer it is waiting for.",
    ),
];

/// `work_attention_items.kind` for the sticky item raised when a work item
/// has no repo resolution. Defined here rather than next to its producer so
/// the registry above does not have to depend on a `pub(crate)` symbol;
/// `record_repo_unresolved_attention` binds *this* constant rather than
/// re-spelling the string, so there is exactly one definition of the value
/// the reconciler matches on.
pub const REPO_UNRESOLVED_ATTENTION_KIND: &str = "repo_unresolved";
/// `work_attention_items.kind` for the item recording an engine
/// auto-archival of a moot revision. Bound by
/// `record_revision_archived_attention`; see
/// [`REPO_UNRESOLVED_ATTENTION_KIND`] for why it lives here.
pub const REVISION_ARCHIVED_ATTENTION_KIND: &str = "revision_archived";
/// Attention-group kind for proposed followup work awaiting the operator's
/// batch-accept gesture.
pub const FOLLOWUP_ATTENTION_KIND: &str = "followup";
/// Attention-group kind for a question posed to the operator.
pub const QUESTION_ATTENTION_KIND: &str = "question";
/// Product-scoped external-tracker fetch failures, resolved by the
/// reconcile loop on the next successful fetch. The reconcile loop
/// (`external_tracker::reconcile::logic`) both raises and resolves against
/// these constants — the kind string is defined once, here.
pub const EXTERNAL_TRACKER_AUTH_FAILED_ATTENTION_KIND: &str = "external_tracker_auth_failed";
/// See [`EXTERNAL_TRACKER_AUTH_FAILED_ATTENTION_KIND`].
pub const EXTERNAL_TRACKER_TOKEN_REVOKED_ATTENTION_KIND: &str = "external_tracker_token_revoked";
/// See [`EXTERNAL_TRACKER_AUTH_FAILED_ATTENTION_KIND`].
pub const EXTERNAL_TRACKER_TRANSIENT_ERRORS_ATTENTION_KIND: &str = "external_tracker_transient_errors";
/// Product-scoped: an upstream issue vanished from the tracker.
pub const EXTERNAL_TRACKER_REMOVED_UPSTREAM_ATTENTION_KIND: &str = "external_tracker_removed_upstream";
/// Product-scoped: the tracker refused a write for permission reasons.
pub const EXTERNAL_TRACKER_PERMISSION_DENIED_ATTENTION_KIND: &str = "external_tracker_permission_denied";

/// The declared lifecycle for `kind`, or `None` when the kind is not
/// registered. Callers treat `None` as "no automatic rule applies" — the
/// reconciler never touches an unregistered kind, so a missing entry can
/// only ever fail closed (the signal stays visible), never open.
pub fn lifecycle_for(kind: &str) -> Option<&'static AttentionLifecycle> {
    ATTENTION_LIFECYCLES.iter().find(|entry| entry.kind == kind)
}

/// Every kind the generic reconciler acts on, paired with its rule. The
/// reconciler iterates this rather than `ATTENTION_LIFECYCLES` so
/// producer-owned and human-owned kinds are never even queried.
pub fn automatically_cleared() -> impl Iterator<Item = &'static AttentionLifecycle> {
    ATTENTION_LIFECYCLES
        .iter()
        .filter(|entry| entry.cleared_by.is_automatic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_registered_kind_is_declared_once() {
        let mut seen = HashSet::new();
        for entry in ATTENTION_LIFECYCLES {
            assert!(
                seen.insert(entry.kind),
                "attention kind {} is declared more than once",
                entry.kind,
            );
        }
    }

    #[test]
    fn every_entry_carries_a_rationale() {
        for entry in ATTENTION_LIFECYCLES {
            assert!(
                entry.rationale.len() > 40,
                "attention kind {} needs a real rationale, not `{}` — the whole point of the table \
                 is that a future reader can tell a deliberate 'human only' from a forgotten one",
                entry.kind,
                entry.rationale,
            );
        }
    }

    /// Names every attention-kind constant the crate defines. A kind added
    /// without a lifecycle entry fails here; a kind renamed or deleted fails
    /// to compile; and the `assert_eq!` on lengths catches an entry added to
    /// the registry but not named here (or vice versa).
    ///
    /// Its honest limit: `declared` is hand-maintained, so a brand-new kind
    /// constant added to *neither* this list nor the registry still passes.
    /// That gap is what `crate::work::warn_if_lifecycle_undeclared` covers
    /// at runtime — it fires on the actual filing call, which no amount of
    /// list curation can be forgotten out of.
    #[test]
    fn every_attention_kind_constant_in_the_crate_is_registered() {
        let declared: &[&str] = &[
            crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND,
            crate::work::DISPATCH_STAGE_STALLED_ATTENTION_KIND,
            crate::work::ATTENTION_KIND_RECOVERY_PERMANENT,
            crate::work::ATTENTION_KIND_RECOVERY_EXHAUSTED,
            crate::coordinator::CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
            crate::dead_pid_sweep::PANE_DEATH_ATTENTION_KIND,
            crate::remote_lease_reconcile::REMOTE_WORKER_DIED_ATTENTION_KIND,
            crate::tmux_adoption::TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND,
            crate::spawn_ack_sweep::DRIVER_START_ATTENTION_KIND,
            crate::app::readoption::PROGRESS_INGRESS_UNRECOVERABLE_ATTENTION_KIND,
            crate::app::probes::PROBE_UNDELIVERED_ATTENTION_KIND,
            crate::completion::NUDGE_BREAKER_ATTENTION_KIND,
            crate::completion::REVIEW_RESULT_GIVEUP_ATTENTION_KIND,
            crate::completion::DRIVER_TERMINAL_ERROR_ATTENTION_KIND,
            crate::completion::REVISION_NO_OP_ATTENTION_KIND,
            crate::completion::MID_TURN_REAP_ATTENTION_KIND,
            crate::coordinator::PANE_SPAWN_FAILED_ATTENTION_KIND,
            crate::coordinator::ANSWER_AGENT_READY_AGE_ATTENTION_KIND,
            crate::stale_worker_sweep::STALE_WORKER_ATTENTION_KIND,
            crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND,
            crate::worker_escalation::WORKER_ESCALATION_ATTENTION_KIND,
            crate::worker_escalation::WORKER_BLOCKED_ATTENTION_KIND,
            crate::abandoned_branch_pr_sweep::ATTENTION_KIND_ABANDONED_BRANCH_NO_PR,
            crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND,
            crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND,
            crate::merge_mechanism::PUSH_RESTRICTION_ATTENTION_KIND,
            crate::envelope_watch::ENVELOPE_OVERRUN_ATTENTION_KIND,
            crate::codex_unobserved_command::UNOBSERVED_COMMAND_ATTENTION_KIND,
            crate::codex_unobserved_command::UNOBSERVED_COMMAND_OVERFLOW_ATTENTION_KIND,
            crate::husk_pane_sweep::HUSK_BREAKER_ATTENTION_KIND,
            crate::answer_agent_completion_sweep::ANSWER_AGENT_STRANDED_ATTENTION_KIND,
            crate::proposal_channel_error::PROPOSAL_CHANNEL_ERROR_ATTENTION_KIND,
            crate::spawn_health::SPAWN_CAPABILITY_ATTENTION_KIND,
            crate::ci_watch::CI_REMEDIATION_EXHAUSTED_ATTENTION_KIND,
            crate::trunk_queue_poller::TRUNK_QUEUE_UNREACHABLE_ATTENTION_KIND,
            crate::trunk_queue_poller::TRUNK_TOKEN_REJECTED_ATTENTION_KIND,
            crate::trunk_queue_poller::TRUNK_QUEUE_NOT_RUNNING_ATTENTION_KIND,
            crate::trunk_queue_poller::TRUNK_QUEUE_ENTRY_CANCELLED_ATTENTION_KIND,
            crate::trunk_queue_poller::TRUNK_QUEUE_MERGE_FAILURE_ATTENTION_KIND,
            crate::trunk_queue_poller::TRUNK_QUEUE_RESUBMIT_STALLED_ATTENTION_KIND,
            REPO_UNRESOLVED_ATTENTION_KIND,
            REVISION_ARCHIVED_ATTENTION_KIND,
            FOLLOWUP_ATTENTION_KIND,
            QUESTION_ATTENTION_KIND,
            EXTERNAL_TRACKER_AUTH_FAILED_ATTENTION_KIND,
            EXTERNAL_TRACKER_TOKEN_REVOKED_ATTENTION_KIND,
            EXTERNAL_TRACKER_TRANSIENT_ERRORS_ATTENTION_KIND,
            EXTERNAL_TRACKER_REMOVED_UPSTREAM_ATTENTION_KIND,
            EXTERNAL_TRACKER_PERMISSION_DENIED_ATTENTION_KIND,
        ];
        for kind in declared {
            assert!(
                lifecycle_for(kind).is_some(),
                "attention kind `{kind}` has no entry in ATTENTION_LIFECYCLES — every signal the \
                 engine can raise must declare what lowers it, even if the answer is \
                 ClearedBy::HumanDecision",
            );
        }
        assert_eq!(
            declared.len(),
            ATTENTION_LIFECYCLES.len(),
            "ATTENTION_LIFECYCLES has entries this test does not name (or vice versa)",
        );
    }

    #[test]
    fn only_the_two_evidence_variants_are_automatic() {
        assert!(ClearedBy::WorkResumed.is_automatic());
        assert!(ClearedBy::ExecutionKindCompleted(ExecutionKind::PrReview).is_automatic());
        assert!(!ClearedBy::ProducerReconciles.is_automatic());
        assert!(!ClearedBy::HumanDecision.is_automatic());
    }

    #[test]
    fn the_dispatch_gate_kinds_are_never_auto_resolved() {
        // These two are consulted by `list_orphan_active_candidates` as an
        // exclusion. Auto-resolving them would silently re-open automatic
        // redispatch for a work item a human was asked to look at.
        for kind in [
            crate::work::ATTENTION_KIND_RECOVERY_PERMANENT,
            crate::work::ATTENTION_KIND_RECOVERY_EXHAUSTED,
        ] {
            let lifecycle = lifecycle_for(kind).expect("registered");
            assert!(
                !lifecycle.cleared_by.is_automatic(),
                "{kind} gates redispatch and must not be auto-resolved",
            );
        }
    }

    #[test]
    fn unregistered_kinds_have_no_lifecycle() {
        assert!(lifecycle_for("something_nobody_declared").is_none());
    }
}
