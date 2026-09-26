//! Bind guide submissions to the socket-attributed execution's active attempt.

use std::path::Path;

use sha2::{Digest, Sha256};

use super::proposal_apply::{ApplyDecision, ApplyOutcome};
use super::*;

pub(super) fn accept(
    tx: &rusqlite::Transaction<'_>,
    execution_id: &str,
    payload_json: &str,
    artifact_root: &Path,
) -> Result<ApplyDecision> {
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
    };
    let payload: boss_protocol::ReviewGuideProposalPayload = match serde_json::from_str(payload_json) {
        Ok(payload) => payload,
        Err(err) => {
            return Ok(ApplyDecision::Rejected(format!("invalid review-guide payload: {err}")));
        }
    };
    let comparison_id: String = tx.query_row(
        "SELECT work_item_id FROM work_executions WHERE id = ?1",
        [execution_id],
        |row| row.get(0),
    )?;
    let packet = match comparison_packet_in_tx(tx, &comparison_id, artifact_root) {
        Ok(Some(packet)) => packet,
        Ok(None) => {
            return Ok(ApplyDecision::Rejected(
                "the comparison this attempt was generated from is gone".into(),
            ));
        }
        Err(err) => {
            return Ok(ApplyDecision::Rejected(format!(
                "could not load the comparison for validation: {err:#}"
            )));
        }
    };
    if let Err(issues) = boss_review_guide::validate_guide_output(&payload.body_markdown, &packet) {
        let detail = issues
            .iter()
            .map(|issue| issue.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Ok(ApplyDecision::Rejected(detail));
    }
    Ok(ApplyDecision::Applied(ApplyOutcome {
        applied_ref: Some(attempt),
        post_commit_audit_line: None,
        review_batch_quorum_outcome: None,
    }))
}

fn comparison_packet_in_tx(
    tx: &rusqlite::Transaction<'_>,
    comparison_id: &str,
    artifact_root: &Path,
) -> Result<Option<boss_pr_review_sources::SourcePacket>> {
    let row: Option<(Option<String>, String)> = tx
        .query_row(
            "SELECT packet_path, packet_hash FROM pr_review_guide_source_comparisons WHERE id = ?1",
            [comparison_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((packet_path, packet_hash)) = row else {
        return Ok(None);
    };
    let packet_path = packet_path
        .filter(|path| !path.is_empty())
        .context("missing referenced source packet blob: no artifact path")?;
    let path = artifact_root.join(packet_path);
    let bytes =
        std::fs::read(&path).with_context(|| format!("missing referenced source packet blob at {}", path.display()))?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == packet_hash,
        "source packet integrity failure at {}: digest mismatch",
        path.display()
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse source packet blob at {}", path.display()))
        .map(Some)
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
