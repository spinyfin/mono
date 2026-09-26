//! Bind guide submissions to the socket-attributed execution's active attempt.

use super::proposal_apply::{ApplyDecision, ApplyOutcome};
use super::*;

type PacketReference = (String, Option<String>, String);

pub(super) struct PreparedGuide {
    reference: PacketReference,
    validation: std::result::Result<(), String>,
}

/// Load and validate without holding the connection mutex or a write transaction.
pub(super) fn prepare(db: &WorkDb, execution_id: &str, payload_json: &str) -> Result<PreparedGuide> {
    let reference = {
        let conn = db.connect()?;
        packet_reference(&conn, execution_id)?.context("the comparison this attempt was generated from is gone")?
    };
    let packet = super::review_guide_sources::load_packet(&db.artifact_root()?, reference.1.as_deref(), &reference.2)?;
    let payload: boss_protocol::ReviewGuideProposalPayload = serde_json::from_str(payload_json)?;
    let validation = boss_review_guide::validate_guide_output(&payload.body_markdown, &packet)
        .map(|_| ())
        .map_err(|issues| issues.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "));
    Ok(PreparedGuide { reference, validation })
}

fn packet_reference(conn: &rusqlite::Connection, execution_id: &str) -> Result<Option<PacketReference>> {
    Ok(conn
        .query_row(
            "SELECT c.id, c.packet_path, c.packet_hash
         FROM pr_review_guide_source_comparisons c
         JOIN work_executions e ON e.work_item_id = c.id WHERE e.id = ?1",
            [execution_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

pub(super) fn accept(
    tx: &rusqlite::Transaction<'_>,
    execution_id: &str,
    prepared: &Result<PreparedGuide>,
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
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(err) => {
            return Ok(ApplyDecision::Rejected(format!(
                "could not load the comparison for validation: {err:#}"
            )));
        }
    };
    // The packet can be replaced or removed while validation runs outside
    // the transaction; accept only the exact comparison we validated.
    if packet_reference(tx, execution_id)?.as_ref() != Some(&prepared.reference) {
        return Ok(ApplyDecision::Rejected(
            "the comparison changed during validation; retry submission".into(),
        ));
    }
    if let Err(detail) = &prepared.validation {
        return Ok(ApplyDecision::Rejected(detail.clone()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_active_chore, create_product, open_db, review_guide_source_packet};
    use crate::work::review_guide_sources::BEFORE_PACKET_READ;

    #[test]
    fn submission_reads_large_packet_without_locks_and_rechecks_concurrent_changes() {
        for mutation in [None, Some("packet"), Some("attempt")] {
            let (_dir, db) = open_db();
            let db = std::sync::Arc::new(db);
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "guide submission");
            let mut packet = review_guide_source_packet("base", "head");
            packet.body = Some("x".repeat(4 * 1024 * 1024));
            let PrSourceCapturePersistOutcome::Stored(capture) = db
                .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet)
                .unwrap()
            else {
                panic!("expected capture")
            };
            let attempt = db
                .create_pr_review_guide_attempt(
                    &capture.series_id,
                    &capture.comparison_id,
                    boss_review_guide::PROMPT_VERSION,
                )
                .unwrap();
            let execution = db
                .create_pr_review_guide_execution(&capture.comparison_id, "acme/widget")
                .unwrap();
            db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
                .unwrap();
            db.start_execution_run(&execution.id, "review", "mono", "lease", "ws", "/tmp/ws")
                .unwrap();
            let hook_db = db.clone();
            let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let hook_observed = observed.clone();
            BEFORE_PACKET_READ.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    drop(
                        hook_db
                            .conn
                            .try_lock()
                            .expect("packet read must release connection mutex"),
                    );
                    let mut conn = hook_db.connect_new().unwrap();
                    conn.busy_timeout(std::time::Duration::ZERO).unwrap();
                    let tx = conn
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                        .expect("packet read must not hold SQLite write lock");
                    match mutation {
                        Some("packet") => {
                            tx.execute(
                                "UPDATE pr_review_guide_source_comparisons SET packet_hash = 'changed'",
                                [],
                            )
                            .unwrap();
                        }
                        Some("attempt") => {
                            tx.execute("UPDATE pr_review_guide_attempts SET status = 'cancelled'", [])
                                .unwrap();
                        }
                        _ => {}
                    }
                    tx.commit().unwrap();
                    hook_observed.store(true, std::sync::atomic::Ordering::SeqCst);
                }))
            });
            let payload =
                serde_json::json!({"body_markdown": "# Guide\n## Problem\n## Implementation\n## Example\n## Review"})
                    .to_string();
            let start = std::time::Instant::now();
            let outcome = db
                .submit_worker_proposal(SubmitWorkerProposalInput {
                    execution_id: &execution.id,
                    work_item_id: &capture.comparison_id,
                    kind: boss_protocol::ProposalKind::ReviewGuide,
                    payload_json: &payload,
                    idempotency_key: "guide",
                })
                .unwrap()
                .unwrap();
            eprintln!("4 MiB packet submission, mutation={mutation:?}: {:?}", start.elapsed());
            assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
            match mutation {
                None => assert_eq!(outcome.proposal.state, boss_protocol::ProposalState::Applied),
                Some(_) => {
                    assert_eq!(outcome.proposal.state, boss_protocol::ProposalState::Rejected);
                    let reason = outcome.proposal.decision_reason.unwrap();
                    assert!(
                        reason.contains("changed during validation")
                            || reason.contains("no running review-guide attempt"),
                        "{reason}"
                    );
                }
            }
        }
    }
}
