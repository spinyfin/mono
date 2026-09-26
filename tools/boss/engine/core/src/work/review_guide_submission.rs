//! Bind guide submissions to the socket-attributed execution's active attempt.

use super::proposal_apply::{ApplyDecision, ApplyOutcome};
use super::*;

pub(super) fn accept(tx: &rusqlite::Transaction<'_>, execution_id: &str) -> Result<ApplyDecision> {
    let attempt: Option<String> = tx
        .query_row(
            "SELECT a.id FROM pr_review_guide_attempts a
         JOIN work_executions e ON e.id = a.execution_id
         WHERE a.execution_id = ?1 AND e.kind = 'pr_review_guide'
           AND a.status = 'running' AND e.status = 'running'",
            [execution_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(attempt) = attempt else {
        return Ok(ApplyDecision::Rejected(
            "this execution has no running review-guide attempt".into(),
        ));
    };
    let already_accepted: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM worker_proposals WHERE execution_id = ?1
         AND kind = 'review_guide' AND state = 'applied')",
        [execution_id],
        |row| row.get(0),
    )?;
    if already_accepted {
        return Ok(ApplyDecision::Rejected(
            "this execution already submitted its review guide".into(),
        ));
    }
    Ok(ApplyDecision::Applied(ApplyOutcome {
        applied_ref: Some(attempt),
        post_commit_audit_line: None,
        review_batch_quorum_outcome: None,
    }))
}

impl WorkDb {
    pub(crate) fn submitted_review_guide(&self, execution_id: &str, attempt_id: &str) -> Result<Option<String>> {
        let proposals =
            self.list_worker_proposals_for_execution(execution_id, boss_protocol::ProposalKind::ReviewGuide)?;
        proposals
            .into_iter()
            .find(|proposal| {
                proposal.state == boss_protocol::ProposalState::Applied
                    && proposal.applied_ref.as_deref() == Some(attempt_id)
            })
            .map(|proposal| {
                let payload: boss_protocol::ReviewGuideProposalPayload = serde_json::from_str(&proposal.payload_json)?;
                Ok(payload.body_markdown)
            })
            .transpose()
    }
}
