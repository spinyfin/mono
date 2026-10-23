//! Detection can synchronously abandon an attempt or attach a revision.
//! Publish the resulting row, rather than the snapshot from before dispatch.
use crate::work::WorkDb;
use boss_protocol::{CiRemediation, ConflictResolution, FrontendEvent};

pub(crate) fn conflict_detected(db: &WorkDb, original: &ConflictResolution) -> FrontendEvent {
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
