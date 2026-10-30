//! Re-exports the storage-values attention lifecycle registry and kinds.
//! Hosts the cross-module test that checks producer registration.

pub use boss_engine_storage_values::attention_kinds::{
    COMPLETION_WITH_BOUND_PR_ATTENTION_KIND, EXTERNAL_TRACKER_AUTH_FAILED_ATTENTION_KIND,
    EXTERNAL_TRACKER_PERMISSION_DENIED_ATTENTION_KIND, EXTERNAL_TRACKER_REMOVED_UPSTREAM_ATTENTION_KIND,
    EXTERNAL_TRACKER_TOKEN_REVOKED_ATTENTION_KIND, EXTERNAL_TRACKER_TRANSIENT_ERRORS_ATTENTION_KIND,
    FOLLOWUP_ATTENTION_KIND, LEGACY_PANE_DEATH_RECONCILE_ATTENTION_KIND, QUESTION_ATTENTION_KIND,
    REPO_UNRESOLVED_ATTENTION_KIND, REVIEW_DESIGN_DOC_UNRESOLVED_ATTENTION_KIND, REVISION_ARCHIVED_ATTENTION_KIND,
};
pub use boss_engine_storage_values::attention_lifecycle::{
    ATTENTION_LIFECYCLES, AttentionLifecycle, ClearedBy, automatically_cleared, lifecycle_for,
};

#[cfg(test)]
mod tests {
    use super::*;

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
            crate::pool_claim_sweep::TEARDOWN_ATTENTION_KIND,
            crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND,
            crate::conflict_ladder::LOCKFILE_REGENERATION_FAILED_ATTENTION_KIND,
            crate::work::DISPATCH_STAGE_STALLED_ATTENTION_KIND,
            crate::work::ATTENTION_KIND_RECOVERY_PERMANENT,
            crate::work::ATTENTION_KIND_RECOVERY_EXHAUSTED,
            crate::coordinator::CHAIN_SERIALIZED_STALL_ATTENTION_KIND,
            LEGACY_PANE_DEATH_RECONCILE_ATTENTION_KIND,
            crate::remote_lease_reconcile::REMOTE_WORKER_DIED_ATTENTION_KIND,
            crate::tmux_adoption::TMUX_ADOPTION_SCHEMA_SKEW_ATTENTION_KIND,
            crate::tmux_adoption::TMUX_LEGACY_LABEL_SERVER_ATTENTION_KIND,
            crate::spawn_ack_sweep::DRIVER_START_ATTENTION_KIND,
            crate::spawn_ack_sweep::LIVENESS_UNDETERMINABLE_ATTENTION_KIND,
            crate::app::readoption::PROGRESS_INGRESS_UNRECOVERABLE_ATTENTION_KIND,
            crate::app::probes::PROBE_UNDELIVERED_ATTENTION_KIND,
            crate::completion::NUDGE_BREAKER_ATTENTION_KIND,
            crate::completion::RUN_DONE_BLOCKED_ATTENTION_KIND,
            crate::completion::REVIEW_RESULT_GIVEUP_ATTENTION_KIND,
            crate::completion::DRIVER_TERMINAL_ERROR_ATTENTION_KIND,
            crate::completion::REVISION_NO_OP_ATTENTION_KIND,
            crate::completion::MID_TURN_REAP_ATTENTION_KIND,
            crate::coordinator::PANE_SPAWN_FAILED_ATTENTION_KIND,
            crate::coordinator::ANSWER_AGENT_READY_AGE_ATTENTION_KIND,
            crate::local_worker_quarantine::ATTENTION_KIND,
            crate::stale_worker_sweep::STALE_WORKER_ATTENTION_KIND,
            crate::hung_worker::HUNG_WORKER_ATTENTION_KIND,
            crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND,
            crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND,
            REVIEW_DESIGN_DOC_UNRESOLVED_ATTENTION_KIND,
            crate::work::PR_REVIEW_BATCH_STALE_ATTENTION_KIND,
            crate::work::PR_REVIEW_REPORTED_MEMBER_LIVE_ATTENTION_KIND,
            crate::worker_escalation::WORKER_ESCALATION_ATTENTION_KIND,
            crate::worker_escalation::WORKER_BLOCKED_ATTENTION_KIND,
            crate::abandoned_branch_pr_sweep::ATTENTION_KIND_ABANDONED_BRANCH_NO_PR,
            crate::workspace_lease_release::LEASE_UNRELEASED_ATTENTION_KIND,
            crate::workspace_lease_release::LEASE_RELEASE_FAILED_ATTENTION_KIND,
            crate::execution_bookmark_recovery::CREATE_FAILED,
            crate::execution_bookmark_recovery::CHAIN_BLOCKED,
            crate::execution_bookmark_recovery::RECOVERY_FAILED,
            crate::execution_bookmark_recovery::RECOVERY_RETIRED,
            crate::abandoned_execution_bookmarks::RECOVERABLE_WORK,
            crate::execution_bookmark_prune::UNPROVEN,
            crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND,
            crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND,
            crate::work::REVIEW_VERDICT_BATCH_NOT_APPLYING_ATTENTION_KIND,
            crate::merge_mechanism::PUSH_RESTRICTION_ATTENTION_KIND,
            crate::envelope_watch::ENVELOPE_OVERRUN_ATTENTION_KIND,
            crate::codex_unobserved_command::UNOBSERVED_COMMAND_ATTENTION_KIND,
            crate::codex_unobserved_command::UNOBSERVED_COMMAND_OVERFLOW_ATTENTION_KIND,
            crate::husk_pane_sweep::HUSK_BREAKER_ATTENTION_KIND,
            crate::answer_agent_completion_sweep::ANSWER_AGENT_STRANDED_ATTENTION_KIND,
            crate::proposal_channel_error::PROPOSAL_CHANNEL_ERROR_ATTENTION_KIND,
            crate::spawn_health::SPAWN_CAPABILITY_ATTENTION_KIND,
            crate::ci_watch::CI_REMEDIATION_EXHAUSTED_ATTENTION_KIND,
            crate::queue_ci_contract::SNAPSHOT_ERROR_ATTENTION_KIND,
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
            COMPLETION_WITH_BOUND_PR_ATTENTION_KIND,
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
}
