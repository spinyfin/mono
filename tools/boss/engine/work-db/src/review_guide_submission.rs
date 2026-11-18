//! Bind guide submissions to the socket-attributed execution's active attempt.
//!
//! Storage never validates a guide body against the pinned workspace: that
//! spawns `git` and belongs above the connection. The caller loads the
//! submission with [`WorkDb::load_review_guide_for_validation`], validates the
//! owned body and packet wherever it likes, and hands the verdict back through
//! [`GuideForValidation::validated`]; [`accept`] then judges the submission
//! against live attempt state inside the proposal transaction.

use super::proposal_apply::{ApplyDecision, ApplyOutcome};
use super::*;

type PacketReference = (String, Option<String>, String);

/// Refusal reason when the attributed execution has no `running` attempt (or
/// is not itself `running`). Shared with
/// [`super::review_guide_recovery`], which recovers the guides that older
/// engines stored as `rejected` rows under exactly this reason.
pub(super) const NO_RUNNING_ATTEMPT_REASON: &str = "this execution has no running review-guide attempt";

/// A submitted guide body and the comparison packet it must satisfy, loaded
/// without holding the connection mutex or a write transaction. Everything a
/// validator needs is owned here; nothing has been judged yet.
pub struct GuideForValidation {
    reference: PacketReference,
    pub packet: boss_pr_review_sources::SourcePacket,
    pub body_markdown: String,
}

impl GuideForValidation {
    /// Attach the caller's validation verdict. A failure carries its detail so
    /// [`accept`] can refuse the submission with it — after the attempt-state
    /// refusals, which take precedence.
    pub fn validated(self, validation: std::result::Result<(), String>) -> PreparedGuide {
        PreparedGuide {
            reference: self.reference,
            validation,
        }
    }
}

/// A submission whose body has been validated by the caller, ready for
/// [`accept`] inside the proposal transaction.
pub struct PreparedGuide {
    reference: PacketReference,
    validation: std::result::Result<(), String>,
}

/// Load the submission for validation without holding the connection mutex or
/// a write transaction.
fn load_for_validation(db: &WorkDb, execution_id: &str, payload_json: &str) -> Result<GuideForValidation> {
    let reference = {
        let conn = db.connect()?;
        packet_reference(&conn, execution_id)?.context("the comparison this attempt was generated from is gone")?
    };
    let packet = super::review_guide_sources::load_packet(&db.artifact_root()?, reference.1.as_deref(), &reference.2)?;
    let payload: boss_protocol::ReviewGuideProposalPayload = serde_json::from_str(payload_json)?;
    Ok(GuideForValidation {
        reference,
        packet,
        body_markdown: payload.body_markdown,
    })
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
        return Ok(ApplyDecision::Rejected(NO_RUNNING_ATTEMPT_REASON.into()));
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

#[work_db_boundary::instrument]
impl WorkDb {
    /// Load the guide a worker submitted, with the packet it must satisfy, for
    /// validation above storage. The packet read can be large, so it holds no
    /// lock; [`accept`] re-checks the packet reference inside the transaction
    /// and refuses a submission whose comparison changed in between.
    pub fn load_review_guide_for_validation(
        &self,
        execution_id: &str,
        payload_json: &str,
    ) -> Result<GuideForValidation> {
        load_for_validation(self, execution_id, payload_json)
    }

    pub fn submitted_review_guide(&self, execution_id: &str, attempt_id: &str) -> Result<Option<String>> {
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
    use crate::review_guide_sources::BEFORE_PACKET_READ;
    use crate::test_support::{create_active_chore, create_product, open_db, review_guide_source_packet};

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
                serde_json::json!({"body_markdown": "# Guide\n## Orientation\nA change.\n## Problem\n## Implementation\n## Example\n## Review\n## Evidence\nNo validation results reported; no screenshots registered."})
                    .to_string();
            let start = std::time::Instant::now();
            let outcome = submit_validated_guide(
                &db,
                SubmitWorkerProposalInput {
                    execution_id: &execution.id,
                    work_item_id: &capture.comparison_id,
                    kind: boss_protocol::ProposalKind::ReviewGuide,
                    payload_json: &payload,
                    idempotency_key: "guide",
                },
                Ok(()),
            )
            .unwrap();
            eprintln!("4 MiB packet submission, mutation={mutation:?}: {:?}", start.elapsed());
            assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
            match (mutation, outcome) {
                (None, Ok(outcome)) => assert_eq!(outcome.proposal.state, boss_protocol::ProposalState::Applied),
                (Some(_), Err(refusal)) => {
                    // A concurrent change is refused before anything is
                    // stored, so the worker's retry is a fresh submission.
                    assert_eq!(refusal.code, boss_protocol::ProposalErrorCode::NotAccepted);
                    assert!(
                        refusal.message.contains("changed during validation")
                            || refusal.message.contains(NO_RUNNING_ATTEMPT_REASON),
                        "{}",
                        refusal.message
                    );
                    assert_eq!(stored_proposal_count(&db, &execution.id), 0);
                }
                (mutation, _) => panic!("unexpected submission outcome for mutation {mutation:?}"),
            }
        }
    }

    /// Submit a review guide the way the engine does, with the validation
    /// step supplied by the test: `Ok(())` stands for a body the workspace
    /// validator accepted, `Err(detail)` for one it refused.
    fn submit_validated_guide(
        db: &WorkDb,
        input: SubmitWorkerProposalInput<'_>,
        validation: std::result::Result<(), String>,
    ) -> Result<std::result::Result<SubmitWorkerProposalOutcome, boss_protocol::ProposalSubmissionError>> {
        let guide = db
            .load_review_guide_for_validation(input.execution_id, input.payload_json)
            .map(|guide| guide.validated(validation));
        db.submit_worker_proposal_with_guide(input, Some(&guide))
    }

    fn stored_proposal_count(db: &WorkDb, execution_id: &str) -> i64 {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM worker_proposals WHERE execution_id = ?1",
                [execution_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    const VALID_GUIDE: &str = "# Guide\n## Orientation\nA change.\n## Problem\n## Implementation\n## Example\n## Review\n## Evidence\nNo validation results reported; no screenshots registered.";

    /// A guide submitted while its attempt is closed is refused without a
    /// stored row, so once re-adoption reopens the attempt the same
    /// submission (same body, same idempotency key) is judged afresh and
    /// accepted — never replayed as the earlier refusal.
    #[test]
    fn refused_submission_stores_nothing_and_the_retry_after_readoption_is_fresh() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "guide submission");
        let (series, comparison) = crate::test_support::seed_review_guide_series(&db, &root);
        let attempt = db
            .create_pr_review_guide_attempt(&series, &comparison, boss_review_guide::PROMPT_VERSION)
            .unwrap();
        let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
        db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
            .unwrap();
        db.start_execution_run(&execution.id, "review", "mono", "lease", "ws", "/tmp/ws")
            .unwrap();
        // The engine wrongly infers the worker dead: the reap orphans the
        // execution and closes the attempt with it.
        db.mark_execution_orphaned(&execution.id, "test: wrongly inferred death")
            .unwrap();
        let payload = serde_json::json!({ "body_markdown": VALID_GUIDE }).to_string();
        let submit = || {
            submit_validated_guide(
                &db,
                SubmitWorkerProposalInput {
                    execution_id: &execution.id,
                    work_item_id: &comparison,
                    kind: boss_protocol::ProposalKind::ReviewGuide,
                    payload_json: &payload,
                    idempotency_key: "guide",
                },
                Ok(()),
            )
            .unwrap()
        };

        let refusal = submit().expect_err("no running attempt must refuse the submission");
        assert_eq!(refusal.code, boss_protocol::ProposalErrorCode::NotAccepted);
        assert_eq!(refusal.message, NO_RUNNING_ATTEMPT_REASON);
        assert_eq!(stored_proposal_count(&db, &execution.id), 0, "a refusal stores nothing");

        // The worker's next hook disproves the death; the attempt comes
        // back with the execution.
        db.readopt_inferred_terminal_execution(&execution.id, "hook_after_terminal", None)
            .unwrap();
        let accepted = submit().expect("the retry is judged on the live state");
        assert!(
            !accepted.already_submitted,
            "nothing was stored, so nothing is replayed"
        );
        assert_eq!(accepted.proposal.state, boss_protocol::ProposalState::Applied);
        assert_eq!(accepted.proposal.applied_ref.as_deref(), Some(attempt.id.as_str()));
        assert_eq!(
            db.submitted_review_guide(&execution.id, &attempt.id)
                .unwrap()
                .as_deref(),
            Some(VALID_GUIDE)
        );
    }

    /// A seeded series with a bound, running guide execution.
    /// Returns `(execution_id, comparison_id, attempt_id)`.
    fn running_guide(db: &WorkDb) -> (String, String, String) {
        let product = create_product(db);
        let root = create_active_chore(db, &product, "guide submission");
        let (series, comparison) = crate::test_support::seed_review_guide_series(db, &root);
        let attempt = db
            .create_pr_review_guide_attempt(&series, &comparison, boss_review_guide::PROMPT_VERSION)
            .unwrap();
        let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
        db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
            .unwrap();
        db.start_execution_run(&execution.id, "review", "mono", "lease", "ws", "/tmp/ws")
            .unwrap();
        (execution.id, comparison, attempt.id)
    }

    fn guide_input<'a>(
        execution_id: &'a str,
        comparison_id: &'a str,
        payload: &'a str,
    ) -> SubmitWorkerProposalInput<'a> {
        SubmitWorkerProposalInput {
            execution_id,
            work_item_id: comparison_id,
            kind: boss_protocol::ProposalKind::ReviewGuide,
            payload_json: payload,
            idempotency_key: "guide",
        }
    }

    /// The caller's validation failure is a refusal, not a stored row: the
    /// worker sees the validator's detail and its retry is judged afresh.
    #[test]
    fn a_body_the_caller_rejected_is_refused_with_its_detail_and_stores_nothing() {
        let (_dir, db) = open_db();
        let (execution, comparison, _attempt) = running_guide(&db);
        let payload = serde_json::json!({ "body_markdown": VALID_GUIDE }).to_string();

        let refusal = submit_validated_guide(
            &db,
            guide_input(&execution, &comparison, &payload),
            Err("caller.rs:L9: linked range does not exist".to_owned()),
        )
        .unwrap()
        .expect_err("a failed validation must refuse the submission");

        assert_eq!(refusal.code, boss_protocol::ProposalErrorCode::NotAccepted);
        assert_eq!(refusal.message, "caller.rs:L9: linked range does not exist");
        assert_eq!(stored_proposal_count(&db, &execution), 0);
    }

    /// Attempt state is judged before the body: an execution with no running
    /// attempt reports that, whatever the validator said, so the recovery
    /// pass that keys on this reason keeps working.
    #[test]
    fn attempt_state_is_refused_before_a_validation_failure_is_reported() {
        let (_dir, db) = open_db();
        let (execution, comparison, _attempt) = running_guide(&db);
        db.mark_execution_orphaned(&execution, "test: inferred death").unwrap();
        let payload = serde_json::json!({ "body_markdown": VALID_GUIDE }).to_string();

        let refusal = submit_validated_guide(
            &db,
            guide_input(&execution, &comparison, &payload),
            Err("would also fail validation".to_owned()),
        )
        .unwrap()
        .expect_err("no running attempt must refuse the submission");

        assert_eq!(refusal.message, NO_RUNNING_ATTEMPT_REASON);
        assert_eq!(stored_proposal_count(&db, &execution), 0);
    }

    /// A load failure is also a refusal that stores nothing: the comparison
    /// the guide must satisfy is gone, so there is nothing to validate against.
    #[test]
    fn a_submission_whose_comparison_cannot_be_loaded_is_refused_and_stores_nothing() {
        let (_dir, db) = open_db();
        let (execution, comparison, _attempt) = running_guide(&db);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_guide_source_comparisons SET packet_hash = 'missing'",
                [],
            )
            .unwrap();
        let payload = serde_json::json!({ "body_markdown": VALID_GUIDE }).to_string();

        let refusal = submit_validated_guide(&db, guide_input(&execution, &comparison, &payload), Ok(()))
            .unwrap()
            .expect_err("an unloadable packet must refuse the submission");

        assert_eq!(refusal.code, boss_protocol::ProposalErrorCode::NotAccepted);
        assert!(
            refusal
                .message
                .starts_with("could not load the comparison for validation"),
            "{}",
            refusal.message
        );
        assert_eq!(stored_proposal_count(&db, &execution), 0);
    }

    /// Storage never judges a guide on its own: a review-guide submission that
    /// reaches the plain entry point without a validation verdict is an error
    /// and stores nothing, so skipping the validation step can never accept a
    /// body by accident.
    #[test]
    fn a_review_guide_submitted_without_a_validation_verdict_is_an_error_and_stores_nothing() {
        let (_dir, db) = open_db();
        let (execution, comparison, _attempt) = running_guide(&db);
        let payload = serde_json::json!({ "body_markdown": VALID_GUIDE }).to_string();

        let error = db
            .submit_worker_proposal(guide_input(&execution, &comparison, &payload))
            .expect_err("an unvalidated review guide must not be accepted");

        assert!(format!("{error:#}").contains("must be validated"), "{error:#}");
        assert_eq!(stored_proposal_count(&db, &execution), 0);
    }
}
