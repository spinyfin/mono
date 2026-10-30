//! Clearing rules for operator-visible attention signals.
//! Automatic resolution requires positive evidence at or after the signal.
//! Database queries and producer reconciliation remain in engine core.

use crate::attention_kinds::*;
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
    /// Whether the engine's `WorkDb::reconcile_stale_attention_signals`
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
/// `work::warn_if_lifecycle_undeclared` in engine core logs a warning for an
/// unregistered kind and is called from *every* filing path — the
/// work-item upsert, the execution-scoped `create_attention_item` /
/// `insert_attention_item_row`, and the two bespoke raw-INSERT helpers.
/// `every_registered_kind_is_declared_once` below pins the table itself.
pub const ATTENTION_LIFECYCLES: &[AttentionLifecycle] = &[
    entry(
        REVISION_CHAIN_RECOVERY_BLOCKED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The PR owner must acknowledge an unrecoverable revision blocking the chain; unrelated root PR review activity does not prove the blocked revision recovered.",
    ),
    entry(
        HUNG_WORKER_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The live-worker sweep resolves warnings when progress resumes or the owning item has no hung or adopting execution.",
    ),
    entry(
        EXECUTION_BOOKMARK_CREATE_FAILED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Dispatch creates the execution recovery bookmark before starting a worker; a subsequent start proves creation succeeded.",
    ),
    entry(
        EXECUTION_BOOKMARK_RECOVERY_FAILED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "A successful recovery/start clears dispatch failures. The bookmark sweep also resolves this signal after inspecting the recorded reference successfully.",
    ),
    entry(
        ABANDONED_EXECUTION_BOOKMARK_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The bookmark sweep resolves this signal when the execution has no remaining unpublished work.",
    ),
    entry(
        EXECUTION_BOOKMARK_PRUNE_UNPROVEN_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The prune sweep resolves this signal once a later pass proves the preserved work published, or a human retires the record after inspecting the commits it names.",
    ),
    entry(
        EXECUTION_BOOKMARK_RECOVERY_RETIRED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The engine already started the item fresh; the superseded commits it names stay in the shared store until a human inspects or discards them, which no later run can prove happened.",
    ),
    entry(
        LEASE_UNRELEASED_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "A release gate kept the lease. The release path resolves the item itself when a later gate \
         pass or sweep returns the workspace; a run starting elsewhere proves nothing about this lease.",
    ),
    entry(
        LEASE_RELEASE_FAILED_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "Cube's release call failed after every gate passed. The leftover-lease sweep retries it and \
         the release path resolves the item once the lease is returned.",
    ),
    // ── Cleared by work resuming ────────────────────────────────────────
    entry(
        LOCKFILE_REGENERATION_FAILED_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "Claims that mechanical lockfile regeneration could not run. The producer lowers it when a \
         mechanical retry succeeds, when the conflict is cleared, or when a new attempt is re-armed \
         — none of those start a run on the parent PR task.",
    ),
    entry(
        CHURN_GUARD_PARKED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "The guard's whole claim is 'we stopped auto-redispatching this item'. A run starting is \
         the redispatch it said did not happen. `request_execution_in_tx_with_live_check` already \
         resolves it on the deliberate-retry path; this covers every other way the item starts.",
    ),
    entry(
        DISPATCH_STAGE_STALLED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts an execution is wedged before dispatch. `dispatch_claimed_execution` resolves it \
         on slot claim; a run start is the same fact observed one step later, and covers the case \
         where the claim path resolved nothing because the engine restarted in between.",
    ),
    entry(
        CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
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
        LEGACY_PANE_DEATH_RECONCILE_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Legacy: filed only by the deleted app-reattach reconcile, so nothing raises this any more. \
         Kept so a row an older engine already persisted still auto-clears on the item's next run \
         start instead of staying open forever after the upgrade.",
    ),
    entry(
        REMOTE_WORKER_DIED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Records that the item's worker died on a remote host and was reaped, and carries the worker's last \
         output so the cause is readable after the workspace goes back to cube. A later run start is the \
         redispatch that reap made room for; if the underlying host problem persists the next death files a \
         fresh item, so nothing is lost by clearing this one.",
    ),
    entry(
        TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Records that boot-time adoption refused and reaped a version-skewed tmux session for \
         the item. A later run start is the redispatch that refusal deliberately made room for.",
    ),
    entry(
        TMUX_LEGACY_LABEL_SERVER_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Records that a worker session is still on the pre-move tmux -L boss server. A later \
         run start means that session has finished and the next worker landed on the durable socket.",
    ),
    entry(
        DRIVER_START_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the worker's driver never started. A later run start is the direct contradiction.",
    ),
    entry(
        LIVENESS_UNDETERMINABLE_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The spawn-ack sweep files this when liveness stays undeterminable across consecutive passes. \
         A later Present probe, a driver-originated signal, or the execution leaving the candidate set \
         (terminal status or a committed reap) is the producer observing that liveness is no longer \
         unknown; the generic reconciler's run-start evidence would leave the warning up for the rest \
         of a recovered run.",
    ),
    entry(
        PROGRESS_INGRESS_UNRECOVERABLE_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts progress for a specific run could not be ingested. A later run supersedes that \
         run entirely, and brings its own ingress.",
    ),
    entry(
        NUDGE_BREAKER_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the auto-nudge loop was bounded for a parked run. A later run start means a fresh \
         run with a fresh breaker; the old park is no longer the item's state.",
    ),
    entry(
        RUN_DONE_BLOCKED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts a run declared `run_done --outcome blocked`, asking a human to adjudicate. A later \
         run start for the same work item — a fresh execution minted by an explicit `bossctl work \
         start` — is the operator's un-park gesture; `work_item_is_deliberately_parked` \
         (`work/dispatch_admission.rs`) additionally scopes its own read of this kind to the \
         latest execution, so the gate itself doesn't wait on this sweep to stop refusing the \
         replacement.",
    ),
    entry(
        DRIVER_TERMINAL_ERROR_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the provider itself failed a run. A later run start is the provider working again \
         for this item. Distinct from `worker_recovery_*` below, which are dispatch *gates* whose \
         resolution is the operator's re-enable gesture, not an observation.",
    ),
    entry(
        PANE_SPAWN_FAILED_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Asserts the worker pane for a run never came up. A later completed spawn run for the same \
         item proves the pane-spawn problem is no longer blocking it; inserting a run before a \
         spawn attempt does not.",
    ),
    entry(
        ANSWER_AGENT_READY_AGE_ATTENTION_KIND,
        ClearedBy::WorkResumed,
        "Names a specific question that waited beyond the answer-agent queue-age threshold. A run start \
         for that question proves the queue wait ended; the scheduler also refreshes this signal while \
         the execution remains ready so an operator can distinguish an active wait from stale history.",
    ),
    // ── Cleared by a later completed pass of the same kind ──────────────
    entry(
        PR_REVIEW_DIED_ATTENTION_KIND,
        ClearedBy::ExecutionKindCompleted(ExecutionKind::PrReview),
        "Asserts a reviewer died before finalizing. `finalize_pr_review_pass` already resolves it \
         inline on every completed pass; this is the backstop for passes finalized while a prior \
         engine process was running, and the shape the rest of the table is modelled on.",
    ),
    // ── The producer owns resolution ────────────────────────────────────
    entry(
        POOL_CLAIM_TEARDOWN_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The pool-claim sweep retries process and viewer teardown and resolves this signal when both are confirmed.",
    ),
    entry(
        LOCAL_WORKER_STARTUP_QUARANTINE_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "Startup re-probes historical local workers after rollback/drain and resolves this attention only when their quarantine clears.",
    ),
    entry(
        STALE_WORKER_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The stale-worker sweep rechecks tmux evidence every pass and resolves this attention when the \
         same worker resumes terminal activity or terminalizes.",
    ),
    entry(
        WORKER_ESCALATION_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The coordinator's probe IS the documented ack gesture; \
         `resolve_worker_signal_attentions_for_execution` fires there. An automatic clear would \
         un-pause the suppressed auto-nudge without the coordinator having answered.",
    ),
    entry(
        WORKER_BLOCKED_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "Same coordinator-ack contract as `worker_escalation`: the marker pauses this run's auto-nudge \
         loop until the coordinator answers, so only the probe may lower it.",
    ),
    entry(
        ATTENTION_KIND_ABANDONED_BRANCH_NO_PR,
        ClearedBy::ProducerReconciles,
        "The sweep re-checks the branch every pass and resolves the item itself once a PR appears \
         — a direct read of the condition, strictly better than any run-start proxy.",
    ),
    entry(
        REVIEW_VERDICT_BATCH_NOT_APPLYING_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The review-verdict applier is both the producer and the only thing that can unstick the \
         batch: it files this when a staged verdict cannot apply because the batch is not \
         `applying`, and resolves it itself on the pass that actually lands the verdict.",
    ),
    entry(
        PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The deferred-admission sweep re-tries create_pre_merge_review_batch every pass and resolves \
         this item itself when a batch is admitted (or an existing live batch/legacy reviewer covers \
         the target). A later unrelated run start does not mean a review slot opened.",
    ),
    entry(
        REVIEW_DESIGN_DOC_UNRESOLVED_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "Filed when assembling a review-brief packet cannot fetch or resolve the project's design \
         doc. Filed and cleared on the review-cycle chain root, not the reviewed revision, so an \
         attention filed while reviewing revision N is cleared by a later successful fetch for \
         revision N+1 or the post-merge review of the root, instead of staying open under N's own id \
         forever. The assembler resolves it on the next successful fetch for that root; a later \
         unrelated run start does not mean GitHub auth, the pointer, or the missing file recovered.",
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
        PROBE_UNDELIVERED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a specific instruction never reached the worker. The run it targeted is over \
         by the time this is filed, so a later run of the same item cannot deliver that probe — it \
         is a different worker with a different prompt. Whether the instruction still needs to be \
         given, and through which channel, is the issuer's call.",
    ),
    entry(
        DEFERRED_SCOPE_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The deferred remainder still exists after any number of successful runs. Closing it is a \
         decision (spin up a followup, or accept the gap) with its own verb, \
         `resolve_deferred_scope_attention`.",
    ),
    entry(
        MERGED_PARENT_DELETION_SIGNOFF_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "It is a gate, not a report: the item is held in `blocked:deletion_signoff` until a human \
         signs off. Auto-resolving would release the hold the item exists to hold.",
    ),
    entry(
        PR_REVIEW_BATCH_STALE_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a review batch was reaped because its cycle root vanished or its members went \
         inert, releasing a global pool reservation. That abandonment already happened; a later \
         successful review of a different PR does not un-do it.",
    ),
    entry(
        PR_REVIEW_REPORTED_MEMBER_LIVE_ATTENTION_KIND,
        ClearedBy::ProducerReconciles,
        "The producer (`file_reported_live_review_batch_member_attentions`) re-evaluates the exact \
         reported+live condition on every recovery-sweep pass and resolves the item itself once the \
         member it named is no longer in that set — teardown finally succeeded, or (for a \
         consolidating supervisor) the report turned out to be within its normal, brief \
         report-then-teardown grace window rather than a stuck pane. A generic run-started rule \
         cannot stand in for this: an unrelated later execution proves nothing about whether THIS \
         member's pane was released.",
    ),
    entry(
        REVIEW_RESULT_GIVEUP_ATTENTION_KIND,
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
        COMPLETION_WITH_BOUND_PR_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records a completion claim contradicted by an owned PR without merge evidence.
         A human must inspect the PR and decide whether the completion claim was justified.",
    ),
    entry(
        REVISION_NO_OP_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a worker declined a reviewer's finding. Whether that judgement stands is the \
         human's call; later runs do not answer it.",
    ),
    entry(
        MID_TURN_REAP_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a revision implementation's worker was torn down while still mid-turn (activity=working) \
         instead of at its own Stop boundary — unpushed work or unrun post-push steps may be lost \
         (incident-004). The execution this happened to is already terminal by the time the item is \
         filed, so no later run of it can supply contradicting evidence; whether the lost work needs \
         redoing is the operator's call, same reasoning as `revision_no_changes_needed`.",
    ),
    entry(
        DIRECT_MERGE_PUSH_RESTRICTION_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "A repository permission the engine cannot change or observe changing. Nothing the engine \
         sees would constitute evidence.",
    ),
    entry(
        ENVELOPE_OVERRUN_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "A cost record. The overrun happened; a later cheap run does not un-spend it.",
    ),
    entry(
        UNOBSERVED_COMMAND_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "An audit record of a command that already ran outside the observed surface. Auto-resolving \
         an audit trail on later success is exactly what an audit trail must not do.",
    ),
    entry(
        UNOBSERVED_COMMAND_OVERFLOW_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Same audit-record reasoning as `codex_unobserved_command`, and the overflow variant additionally \
         records that further unobserved commands went uncounted — a gap no later run fills in.",
    ),
    entry(
        ATTENTION_KIND_RECOVERY_PERMANENT,
        ClearedBy::HumanDecision,
        "This is a dispatch gate, not a report: `list_orphan_active_candidates` excludes items with \
         it open, and its own body tells the operator that resolving it re-enables redispatch. It \
         must stay open until the human says the underlying API problem is fixed.",
    ),
    entry(
        ATTENTION_KIND_RECOVERY_EXHAUSTED,
        ClearedBy::HumanDecision,
        "Same dispatch-gate contract as `worker_recovery_permanent_error`: `list_orphan_active_candidates` \
         excludes items with it open, so resolving it is the operator re-enabling redispatch.",
    ),
    entry(
        HUSK_BREAKER_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "About pane retirement, not about this work item making progress — a new run starting says \
         nothing about whether husk retirement still fails.",
    ),
    entry(
        ANSWER_AGENT_STRANDED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The doc comment the answer agent was dispatched for is still unanswered. An implementation \
         run starting on the same item does not answer it.",
    ),
    entry(
        PROPOSAL_CHANNEL_ERROR_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records a worker proposal the engine could not accept — the proposal's content is lost and \
         needs a human to re-file it. A later run does not recover it.",
    ),
    entry(
        SPAWN_CAPABILITY_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Host-level spawn capability, not per-item state. One item starting elsewhere is not \
         evidence the unhealthy host recovered.",
    ),
    entry(
        QUEUE_CI_CONTRACT_ERROR_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Contract capture failed; an operator must inspect configuration or provider access and re-check the dequeued PR.",
    ),
    entry(
        CI_REMEDIATION_EXHAUSTED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "The attempt budget is spent. Clearing it on a later run would silently hand the item a \
         fresh budget it was never granted.",
    ),
    entry(
        TRUNK_QUEUE_UNREACHABLE_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Queue-level infrastructure state. The poller does not currently resolve these, and a \
         work-item run start is not evidence about the queue — leaving them for the operator is \
         the honest answer until the poller grows a resolve path of its own.",
    ),
    entry(
        TRUNK_TOKEN_REJECTED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "A rejected credential: nothing the engine can observe replaces a token, and a work item \
         starting a run says nothing about whether the Trunk token is valid again.",
    ),
    entry(
        TRUNK_QUEUE_NOT_RUNNING_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Queue-level infrastructure state; see `trunk_queue_unreachable`. A stopped merge queue is \
         restarted by an operator, not by anything this engine can watch happen.",
    ),
    entry(
        TRUNK_QUEUE_ENTRY_CANCELLED_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records that a merge attempt was cancelled out of the queue — a decision to re-submit is \
         the operator's.",
    ),
    entry(
        TRUNK_QUEUE_MERGE_FAILURE_ATTENTION_KIND,
        ClearedBy::HumanDecision,
        "Records a failed merge. The PR is still unmerged until someone acts.",
    ),
    entry(
        TRUNK_QUEUE_RESUBMIT_STALLED_ATTENTION_KIND,
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
        for kind in [ATTENTION_KIND_RECOVERY_PERMANENT, ATTENTION_KIND_RECOVERY_EXHAUSTED] {
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
