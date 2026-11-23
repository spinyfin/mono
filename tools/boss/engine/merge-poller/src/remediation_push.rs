//! Detection can synchronously abandon an attempt or attach a revision.
//! Publish the resulting row, rather than the snapshot from before dispatch.
//!
//! The app keeps the newest attempt row per PR and derives the card's
//! "ci failing" chip from it, so every engine transition that ends an
//! attempt or clears a CI failure must publish the row as it now stands.
//! A clear that does not carry the row leaves the app holding a
//! `pending`/`running` copy that re-raises the chip on its next board
//! refresh, for a head that is no longer current.
use crate::work::{PendingMergeCheck, WorkDb};
use boss_execution_contracts::ExecutionPublisher;
use boss_protocol::{CiRemediation, ConflictResolution, FrontendEvent};

/// Publish a CI attempt row as it currently stands in the database (status,
/// budget and card state), after the engine changed it outside detection.
pub async fn publish_ci_attempt(db: &WorkDb, publisher: &dyn ExecutionPublisher, attempt: &CiRemediation) {
    publisher
        .publish_frontend_event_on_product(&attempt.product_id, ci_detected(db, attempt))
        .await;
}

/// Tell the app the PR's CI failure is cleared, together with the PR's
/// newest attempt row as it now stands. Whatever ended that attempt
/// (superseded by a new head, retriggered, failed) the app's copy then
/// matches the engine, instead of lingering as `running`. Every
/// `CiFailureCleared` goes through here.
pub async fn publish_ci_failure_cleared(
    db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
) {
    let attempt = match db.latest_ci_remediation_for_pr(&candidate.work_item_id, &candidate.pr_url) {
        Ok(latest) => latest.map(|row| db.remediation_entry(row.list_entry())),
        Err(err) => {
            tracing::warn!(
                %err,
                work_item_id = %candidate.work_item_id,
                "could not read the newest CI attempt for a cleared failure",
            );
            None
        }
    };
    publisher
        .publish_frontend_event_on_product(
            &candidate.product_id,
            FrontendEvent::CiFailureCleared {
                product_id: candidate.product_id.clone(),
                work_item_id: candidate.work_item_id.clone(),
                pr_url: candidate.pr_url.clone(),
                attempt,
            },
        )
        .await;
}

pub fn conflict_detected(db: &WorkDb, original: &ConflictResolution) -> FrontendEvent {
    let current = match db.get_conflict_resolution(&original.id) {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(%err, attempt_id = %original.id, "could not refresh conflict push row");
            None
        }
    };
    let current = current.as_ref().unwrap_or(original);
    let attempt = db.remediation_entry(current.list_entry());
    let product_id = current.product_id.clone();
    let work_item_id = current.work_item_id.clone();
    let attempt_id = current.id.clone();
    let pr_url = current.pr_url.clone();
    let failure_reason = current.failure_reason.clone().unwrap_or_default();
    match current.status.as_str() {
        "abandoned" | "superseded" => FrontendEvent::ConflictResolutionAbandoned {
            attempt,
            product_id,
            work_item_id,
            attempt_id,
            pr_url,
            failure_reason,
        },
        "failed" => FrontendEvent::ConflictResolutionFailed {
            attempt,
            product_id,
            work_item_id,
            attempt_id,
            pr_url,
            failure_reason,
        },
        "succeeded" => FrontendEvent::ConflictResolutionSucceeded {
            attempt,
            product_id,
            work_item_id,
            attempt_id,
            pr_url,
        },
        _ => FrontendEvent::ConflictResolutionStarted {
            attempt,
            product_id,
            work_item_id,
            attempt_id,
            pr_url,
        },
    }
}

pub(crate) fn ci_detected(db: &WorkDb, original: &CiRemediation) -> FrontendEvent {
    let current = match db.get_ci_remediation(&original.id) {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(%err, attempt_id = %original.id, "could not refresh CI push row");
            None
        }
    };
    let current = current.as_ref().unwrap_or(original);
    let attempt = db.remediation_entry(current.list_entry());
    let product_id = current.product_id.clone();
    let work_item_id = current.work_item_id.clone();
    let attempt_id = current.id.clone();
    let pr_url = current.pr_url.clone();
    let failure_reason = current.failure_reason.clone().unwrap_or_default();
    match current.status.as_str() {
        "abandoned" | "superseded" => FrontendEvent::CiRemediationAbandoned {
            attempt,
            product_id,
            work_item_id,
            attempt_id,
            pr_url,
            failure_reason,
        },
        "failed" => FrontendEvent::CiRemediationFailed {
            attempt,
            product_id,
            work_item_id,
            attempt_id,
            pr_url,
            failure_reason,
        },
        "succeeded" => FrontendEvent::CiRemediationSucceeded {
            attempt,
            product_id,
            work_item_id,
            attempt_id,
            pr_url,
        },
        _ => FrontendEvent::CiRemediationStarted {
            attempt,
            product_id,
            work_item_id,
            attempt_id,
            pr_url,
            attempt_kind: current.attempt_kind.clone(),
        },
    }
}
