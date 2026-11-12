//! Run-scoped worker writes, independent of transport.
//!
//! A worker's answer reply, guide outcome, conflict/CI failure marks and
//! producer-side conflict record are each one validated database write plus
//! the publishes and metrics that follow it. They are reached two ways: the
//! worker's own RPC (`comments.rs`, `conflict_resolution.rs`,
//! `ci_remediation.rs`) and the hub's projection of a write a spoke queued
//! (`spoke_projection.rs`). Both call the functions here, so a remote worker's
//! write refreshes the UI, announces failures and counts metrics exactly as a
//! local one does, and the two cannot drift.
//!
//! A spoke write passes its [`SpokeWriteApplication`] so the result row commits
//! in the same transaction as the domain write. A transient database fault is
//! [`SpokeWriteError::Retry`]; everything else is a refusal.

use boss_protocol::{
    ArchitecturalMismatchEvidence, CiRemediation, ConflictResolution, FrontendEvent, QueueCiEvidence, WorkComment,
};
use boss_work_db::{ExecutionKind, ProducerConflictInsertInput, SpokeWriteApplication, SpokeWriteError};

use super::{
    ServerState,
    handler_helpers::{publish_comment_invalidation, send_work_error},
    session_queue::SessionSink,
};

/// Who a write's UI invalidation is attributed to: the requesting session for a
/// local RPC, a fixed engine-side origin for a projected spoke write.
#[derive(Clone, Copy)]
pub(super) struct WriteOrigin<'a> {
    pub session_id: &'a str,
    pub request_id: &'a str,
}

impl WriteOrigin<'static> {
    /// The origin of a write the hub applied on a spoke's behalf.
    pub(super) const PROJECTED: Self = Self {
        session_id: "spoke-projection",
        request_id: "",
    };
}

/// Reply to the requester of a failed write.
pub(super) fn send_write_error(sink: &SessionSink, request_id: &str, error: SpokeWriteError) {
    match error {
        SpokeWriteError::Retry(error) => send_work_error(sink, request_id, &error),
        SpokeWriteError::Refused(message) => send_work_error(sink, request_id, message),
    }
}

/// Post the answer agent's reply and publish the comment's new state.
pub(super) async fn answer_reply(
    state: &ServerState,
    origin: WriteOrigin<'_>,
    run_id: &str,
    body: &str,
    application: Option<SpokeWriteApplication<'_>>,
) -> Result<(WorkComment, u64), SpokeWriteError> {
    if body.trim().is_empty() {
        return Err(SpokeWriteError::refused("reply body may not be empty"));
    }
    let execution = state.work_db.get_execution(run_id).map_err(|error| {
        if boss_work_db::retryable_work_error(&error) {
            SpokeWriteError::Retry(error)
        } else {
            SpokeWriteError::refused(format!("unknown run '{run_id}': {error}"))
        }
    })?;
    if execution.kind != ExecutionKind::AnswerAgent {
        return Err(SpokeWriteError::refused(format!(
            "run '{run_id}' is not an answer-agent execution"
        )));
    }
    let comment = state.work_db.apply_answer_agent_reply(run_id, body, application)?;
    boss_engine_completion::answer_agent_observability::record_replied(&state.metrics);
    let revision = publish_comment_invalidation(
        state,
        origin.session_id,
        origin.request_id,
        &comment.artifact_kind,
        &comment.artifact_id,
        "comment_answered",
    )
    .await;
    Ok((comment, revision))
}

/// Record a revision worker's grounded outcome for a guide comment, announce a
/// requested regeneration, and publish the comment's new state.
pub(super) async fn guide_outcome(
    state: &ServerState,
    origin: WriteOrigin<'_>,
    run_id: &str,
    outcome: boss_protocol::GuideCommentOutcome,
    application: Option<SpokeWriteApplication<'_>>,
) -> Result<(WorkComment, u64), SpokeWriteError> {
    if outcome.response.trim().is_empty() {
        return Err(SpokeWriteError::refused("guide outcome body may not be empty"));
    }
    let execution = state
        .work_db
        .get_execution(run_id)
        .map_err(SpokeWriteError::from_work)?;
    if execution.kind != ExecutionKind::RevisionImplementation {
        return Err(SpokeWriteError::refused(format!(
            "run '{run_id}' is not a revision execution"
        )));
    }
    let recorded = state
        .work_db
        .record_guide_comment_outcome_applied(&execution.work_item_id, outcome, application)
        .map_err(SpokeWriteError::from_work)?;
    match recorded.regeneration {
        Some(Ok(boss_work_db::RetryReviewGuideOutcome::Created(_))) => {
            if let Some(root) = state
                .work_db
                .root_task_id_for_review_guide_series(&recorded.comment.artifact_id)
                .ok()
                .flatten()
            {
                boss_engine_completion::execution_publisher::notify_review_guide_changed(
                    &state.work_db,
                    &state.publisher,
                    &root,
                    "review_guide_retry_queued",
                )
                .await;
            }
        }
        Some(Ok(boss_work_db::RetryReviewGuideOutcome::AlreadyRequested(_))) => {
            tracing::info!(
                comment_id = %recorded.comment.id,
                series_id = %recorded.comment.artifact_id,
                "guide outcome regeneration already queued for this revision"
            );
        }
        Some(Ok(boss_work_db::RetryReviewGuideOutcome::NoComparison)) => {
            tracing::warn!(
                comment_id = %recorded.comment.id,
                series_id = %recorded.comment.artifact_id,
                "guide outcome requested regeneration but no source comparison exists"
            );
        }
        Some(Err(error)) => {
            // The outcome itself is already durably recorded; only the
            // regeneration retry failed. Still publish the comment
            // invalidation and report success: surfacing this as an error
            // would tell the caller the whole request failed when the
            // disposition it asked to record is in the database.
            tracing::warn!(
                comment_id = %recorded.comment.id,
                series_id = %recorded.comment.artifact_id,
                %error,
                "guide comment outcome recorded but its regeneration request failed"
            );
        }
        None => {}
    }
    let comment = recorded.comment;
    let revision = publish_comment_invalidation(
        state,
        origin.session_id,
        origin.request_id,
        &comment.artifact_kind,
        &comment.artifact_id,
        "guide_comment_outcome_recorded",
    )
    .await;
    Ok((comment, revision))
}

/// What a conflict mark-failed call did to its attempt.
pub(super) enum ConflictMarked {
    /// The evidence was recorded on the live attempt and the repo owner will be
    /// asked. The attempt is not failed.
    ArchitecturalMismatch(Box<ConflictResolution>),
    /// The attempt is now failed.
    Failed(Box<ConflictResolution>),
}

/// Mark a conflict-resolution attempt failed (or record an architectural
/// mismatch on it) and broadcast the failure.
pub(super) async fn conflict_mark_failed(
    state: &ServerState,
    attempt_id: &str,
    reason: &str,
    evidence: Option<&ArchitecturalMismatchEvidence>,
    application: Option<SpokeWriteApplication<'_>>,
) -> Result<ConflictMarked, SpokeWriteError> {
    let unknown = || {
        SpokeWriteError::refused(format!(
            "conflict resolution attempt {attempt_id:?} is unknown or already terminal"
        ))
    };
    if reason == boss_protocol::CONFLICT_ARCHITECTURAL_MISMATCH_REASON {
        // Not a dead end: the evidence is validated and recorded on the live
        // attempt, and the worker's blocked declaration then raises the
        // rewrite-or-force-a-merge question.
        let attempt = state
            .work_db
            .record_conflict_architectural_mismatch_applied(attempt_id, evidence, application)
            .map_err(SpokeWriteError::from_work)?
            .ok_or_else(unknown)?;
        tracing::warn!(
            attempt_id = %attempt.id,
            work_item_id = %attempt.work_item_id,
            pr_url = %attempt.pr_url,
            "architectural_mismatch recorded; the repo owner will be asked",
        );
        return Ok(ConflictMarked::ArchitecturalMismatch(Box::new(attempt)));
    }
    boss_protocol::validate_conflict_mark_failed_reason(reason).map_err(SpokeWriteError::refused)?;
    // Worker-facing stop-condition surface: the only state change is on a row
    // keyed by an opaque id, so a forged attempt id has no row to clobber.
    // Non-canonical reasons are refused above; engine-internal reasons call
    // `WorkDb::mark_conflict_resolution_failed` directly.
    let attempt = state
        .work_db
        .mark_conflict_resolution_failed_applied(attempt_id, reason, application)
        .map_err(SpokeWriteError::from_work)?
        .ok_or_else(unknown)?;
    tracing::warn!(
        attempt_id = %attempt.id,
        work_item_id = %attempt.work_item_id,
        pr_url = %attempt.pr_url,
        %reason,
        "mark_conflict_resolution_failed: attempt flipped to failed",
    );
    // Typed activity-feed event so subscribers render the failed attempt
    // without round-tripping through the CLI's response.
    state
        .publisher
        .publish_frontend_event_on_product(
            &attempt.product_id,
            FrontendEvent::ConflictResolutionFailed {
                attempt: state.work_db.remediation_entry(attempt.list_entry()),
                product_id: attempt.product_id.clone(),
                work_item_id: attempt.work_item_id.clone(),
                attempt_id: attempt.id.clone(),
                pr_url: attempt.pr_url.clone(),
                failure_reason: reason.to_owned(),
            },
        )
        .await;
    Ok(ConflictMarked::Failed(Box::new(attempt)))
}

/// Mark a CI-remediation attempt failed and broadcast the failure.
pub(super) async fn ci_mark_failed(
    state: &ServerState,
    attempt_id: &str,
    reason: &str,
    queue_ci_evidence: Option<&QueueCiEvidence>,
    application: Option<SpokeWriteApplication<'_>>,
) -> Result<CiRemediation, SpokeWriteError> {
    state
        .work_db
        .validate_queue_ci_failure(attempt_id, queue_ci_evidence, reason)
        .map_err(SpokeWriteError::from_work)?;
    let reason = match queue_ci_evidence {
        Some(evidence) => format!(
            "{reason}\nQueue verification: main_sha={} verified_head_sha={} runs={}",
            evidence.main_sha,
            evidence.verified_head_sha,
            evidence
                .runs
                .iter()
                .map(|run| format!("{}: {} (exit {})", run.check_name, run.command, run.exit_code))
                .collect::<Vec<_>>()
                .join("; ")
        ),
        None => reason.to_owned(),
    };
    let attempt = state
        .work_db
        .mark_ci_remediation_failed_applied(attempt_id, &reason, application)
        .map_err(SpokeWriteError::from_work)?
        .ok_or_else(|| {
            SpokeWriteError::refused(format!(
                "ci_remediation attempt {attempt_id:?} is unknown or already terminal"
            ))
        })?;
    tracing::warn!(
        attempt_id = %attempt.id,
        work_item_id = %attempt.work_item_id,
        pr_url = %attempt.pr_url,
        %reason,
        "mark_ci_remediation_failed: attempt flipped to failed",
    );
    state
        .publisher
        .publish_frontend_event_on_product(
            &attempt.product_id,
            FrontendEvent::CiRemediationFailed {
                attempt: state.work_db.remediation_entry(attempt.list_entry()),
                product_id: attempt.product_id.clone(),
                work_item_id: attempt.work_item_id.clone(),
                attempt_id: attempt.id.clone(),
                pr_url: attempt.pr_url.clone(),
                failure_reason: reason.clone(),
            },
        )
        .await;
    Ok(attempt)
}

/// Record a producer-side conflict a worker resolved and count its class.
pub(super) fn producer_conflict(
    state: &ServerState,
    input: ProducerConflictInsertInput,
) -> Result<ConflictResolution, SpokeWriteError> {
    let attempt = state
        .work_db
        .record_producer_side_conflict(input)
        .map_err(SpokeWriteError::from_work)?;
    tracing::info!(
        attempt_id = %attempt.id,
        work_item_id = %attempt.work_item_id,
        product_id = %attempt.product_id,
        "record_producer_side_conflict: telemetry row recorded",
    );
    if let Some(class) = attempt.conflict_class.as_deref() {
        boss_engine_merge_poller::record_conflict_class_counter(&state.metrics, &attempt.product_id, class);
    }
    Ok(attempt)
}
