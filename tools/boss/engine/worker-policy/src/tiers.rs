//! Shared tiers and exhaustive proposal-kind admission.
use boss_protocol::{ProposalKind, WorkerTierDenial, WorkerTierDenialReason};

use crate::WorkerVerbDecision;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcTier {
    User,
    AppOrBoss,
    BossOnly,
    Worker,
    Progression,
}

/// Every kind must be reviewed for both agent tiers when the vocabulary grows.
/// Progression ships closed; a kind is enabled only alongside its scoped handler.
pub fn proposal_kind_decision(tier: RpcTier, kind: ProposalKind) -> WorkerVerbDecision {
    match kind {
        ProposalKind::Attention
        | ProposalKind::EffortEscalation
        | ProposalKind::Blocked
        | ProposalKind::DeferredScope
        | ProposalKind::FollowupTask
        | ProposalKind::AutomationOutcome
        | ProposalKind::PrCreated
        | ProposalKind::ReviewGuide
        | ProposalKind::ReviewReport
        | ProposalKind::ReviewVerdict
        | ProposalKind::RunDone
        | ProposalKind::Wait => match tier {
            RpcTier::Progression => {
                let mut denial = WorkerTierDenial::closed(
                    format!("SubmitProposal:{}", kind.as_str()),
                    WorkerTierDenialReason::CoordinatorOnly,
                );
                denial.message = format!("Proposal kind `{kind}` is not enabled for progression sessions.");
                WorkerVerbDecision::Deny(Box::new(denial))
            }
            RpcTier::Worker | RpcTier::User | RpcTier::AppOrBoss | RpcTier::BossOnly => WorkerVerbDecision::Allow,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verb_decision;
    use boss_protocol::FrontendRequest;

    #[test]
    fn every_proposal_kind_is_closed_for_progression_and_classified_for_workers() {
        for &kind in ProposalKind::ALL {
            assert!(
                !proposal_kind_decision(RpcTier::Progression, kind).is_allowed(),
                "{kind}"
            );
            assert!(proposal_kind_decision(RpcTier::Worker, kind).is_allowed(), "{kind}");
        }
    }

    #[test]
    fn progression_reads_and_submits_but_cannot_use_sanctioned_worker_writes() {
        for request in [
            FrontendRequest::ListProducts,
            FrontendRequest::SubmitProposal {
                run_id: "exec_scope".into(),
                kind: ProposalKind::Wait,
                payload: serde_json::json!({}),
                idempotency_key: None,
            },
        ] {
            assert!(verb_decision(RpcTier::Progression, &request).is_allowed());
        }
        for request in [
            FrontendRequest::UpdateWorkItem {
                id: "task_scope".into(),
                patch: Default::default(),
            },
            FrontendRequest::CreateAutomationTask {
                automation_id: "auto_scope".into(),
                name: "Followup".into(),
                description: None,
                target_files: vec![],
                target_symbols: vec![],
            },
        ] {
            assert!(!verb_decision(RpcTier::Progression, &request).is_allowed());
        }
    }
}
