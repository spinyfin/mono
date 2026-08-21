//! `FrontendRequest` handlers for screenshot evidence kept for a worker's
//! own verification and for an operator inspecting a run locally:
//! `SubmitAttachment` (write) and `ListAttachments` (read).
//!
//! Both are attributed exactly the way the proposal verbs are — from the
//! socket peer's pid, walked up to a registered worker run — so this module
//! reuses [`super::proposals::attribute_caller`] rather than spelling the
//! chain a second time and letting the two drift. The refusal vocabulary is
//! reused for the same reason: the attribution failure modes are literally
//! identical, so a worker that has learned to read a `ProposalRejected` has
//! learned to read this too.
//!
//! ## The confused-deputy shape, and what closes it
//!
//! `SubmitAttachment` hands the engine a path *the worker chose* and asks it
//! to read it. Left alone that is a privilege-escalation primitive: a worker
//! could name `state.db` and have the engine copy it into a store served over
//! HTTP. Three independent gates close it, none of which relies on the worker
//! being well-behaved:
//!
//! 1. the path is canonicalised and must land inside the *attributed*
//!    execution's own workspace or a temp dir ([`AllowedRoots`]) — and the
//!    execution is peer-resolved, so a worker cannot borrow another run's
//!    workspace by claiming its id;
//! 2. the bytes must sniff as a real PNG or JPEG, which no database, key
//!    file, or transcript does;
//! 3. size, dimensions, and per-run/per-work-item counts are capped, and
//!    every breach is a loud typed refusal rather than a silent truncation.
//!
//! Design: `tools/boss/docs/designs/worker-screenshot-evidence-attachments.md`.

use super::*;

use boss_engine_attachments::{AllowedRoots, IngestRejection};
use boss_protocol::{ProposalErrorCode, ProposalFieldError, ProposalSubmissionError};

use crate::metrics::Registry;
use crate::work::SubmitAttachmentInput;

/// Longest accepted caption. Long enough for a real sentence about what the
/// image shows, short enough that the gallery stays a gallery rather than
/// becoming a place to write the PR description twice.
const MAX_CAPTION_CHARS: usize = 240;

crate::register_counter!(
    ATTACHMENT_STORED,
    "work_attachments.stored",
    "SubmitAttachment ingested an image into the evidence store.",
);
crate::register_counter!(
    ATTACHMENT_REPLAYED,
    "work_attachments.replayed",
    "SubmitAttachment matched bytes this execution had already stored and returned the existing row.",
);
crate::register_counter!(
    ATTACHMENT_VALIDATION_FAILED,
    "work_attachments.validation_failed",
    "SubmitAttachment refused a submission (unreadable/confined-out path, wrong format, oversize, bad dimensions).",
);
crate::register_counter!(
    ATTACHMENT_RATE_LIMITED,
    "work_attachments.rate_limited",
    "SubmitAttachment refused a submission because a per-execution or per-work-item cap was exhausted.",
);

/// Register every attachment counter with `registry`. Called from
/// [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&ATTACHMENT_STORED);
    registry.register_counter(&ATTACHMENT_REPLAYED);
    registry.register_counter(&ATTACHMENT_VALIDATION_FAILED);
    registry.register_counter(&ATTACHMENT_RATE_LIMITED);
}

/// Validate the worker-supplied caption. Absent is fine; blank is normalised
/// to absent; overlong is a refusal rather than a silent trim, on the same
/// principle as every other cap here — the worker should know what got
/// stored.
fn validate_caption(caption: Option<&str>) -> std::result::Result<String, ProposalSubmissionError> {
    let Some(caption) = caption else {
        return Ok(String::new());
    };
    let trimmed = caption.trim();
    if trimmed.chars().count() > MAX_CAPTION_CHARS {
        return Err(ProposalSubmissionError::validation(vec![ProposalFieldError::new(
            "caption",
            format!(
                "caption is {} characters; the cap is {MAX_CAPTION_CHARS}. Say what the image \
                 shows in one line and put the detail in the PR body.",
                trimmed.chars().count()
            ),
        )]));
    }
    Ok(trimmed.to_owned())
}

pub(super) async fn handle_submit_attachment(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::SubmitAttachment { run_id, path, caption } = req else {
        unreachable!()
    };

    let caller = match proposals::attribute_caller(&server_state, &work_db, peer_pid, &run_id) {
        Ok(caller) => caller,
        Err(error) => {
            tracing::warn!(
                run_id = %run_id,
                peer_pid = ?peer_pid,
                code = %error.code,
                "submit_attachment rejected: attribution failed",
            );
            return proposals::send_rejection(&sink, &request_id, error);
        }
    };

    let caption = match validate_caption(caption.as_deref()) {
        Ok(caption) => caption,
        Err(error) => {
            ATTACHMENT_VALIDATION_FAILED.inc(&server_state.metrics);
            return proposals::send_rejection(&sink, &request_id, error);
        }
    };

    // Caps before bytes. Checking here — rather than only inside the insert
    // transaction — means a worker that is looping never causes a single blob
    // to be written, so there is nothing for the orphan sweep to clean up
    // afterwards. The transaction re-checks; this is the pre-flight.
    match work_db.attachment_counts(&caller.execution_id, &caller.work_item_id) {
        Ok((for_execution, for_work_item)) => {
            if let Err(refusal) = crate::work::check_attachment_caps(for_execution, for_work_item) {
                ATTACHMENT_RATE_LIMITED.inc(&server_state.metrics);
                return proposals::send_rejection(&sink, &request_id, refusal);
            }
        }
        Err(err) => {
            tracing::error!(execution_id = %caller.execution_id, ?err, "submit_attachment: cap pre-flight failed");
            return proposals::send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(
                    ProposalErrorCode::Internal,
                    format!("failed to count attachments: {err}"),
                ),
            );
        }
    }

    // The workspace comes from the *attributed* execution's row, never from
    // the request: that is what stops a worker widening its own confinement.
    let workspace_path = work_db
        .get_execution(&caller.execution_id)
        .ok()
        .and_then(|execution| execution.workspace_path)
        .map(std::path::PathBuf::from);
    let allowed = AllowedRoots::for_workspace(workspace_path.as_deref());

    let image = match server_state
        .attachment_store
        .ingest(std::path::Path::new(&path), &allowed)
    {
        Ok(image) => image,
        Err(rejection) => {
            ATTACHMENT_VALIDATION_FAILED.inc(&server_state.metrics);
            tracing::debug!(
                execution_id = %caller.execution_id,
                path = %path,
                rejection = %rejection,
                "submit_attachment rejected: ingest validation failed",
            );
            let error = match rejection {
                // A store write failure is the engine's fault, not the
                // caller's — it must not read as "fix your path and retry".
                IngestRejection::StoreWriteFailed { ref detail } => {
                    ProposalSubmissionError::new(ProposalErrorCode::Internal, detail.clone())
                }
                other => ProposalSubmissionError::validation(vec![other.into_field_error()]),
            };
            return proposals::send_rejection(&sink, &request_id, error);
        }
    };

    let outcome = work_db.submit_work_attachment(SubmitAttachmentInput {
        execution_id: &caller.execution_id,
        work_item_id: &caller.work_item_id,
        image: &image,
        caption: &caption,
    });

    match outcome {
        Ok(Ok(stored)) => {
            if stored.already_stored {
                ATTACHMENT_REPLAYED.inc(&server_state.metrics);
            } else {
                ATTACHMENT_STORED.inc(&server_state.metrics);
            }
            tracing::info!(
                attachment_id = %stored.attachment.id,
                execution_id = %caller.execution_id,
                work_item_id = %caller.work_item_id,
                already_stored = stored.already_stored,
                size_bytes = stored.attachment.size_bytes,
                "attachment stored",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::AttachmentStored {
                    attachment: stored.attachment,
                    already_stored: stored.already_stored,
                    evidence_base_url: server_state.evidence_base_url(),
                },
            );
        }
        Ok(Err(refusal)) => {
            if refusal.code == ProposalErrorCode::RateLimited {
                ATTACHMENT_RATE_LIMITED.inc(&server_state.metrics);
            }
            proposals::send_rejection(&sink, &request_id, refusal);
        }
        Err(err) => {
            tracing::error!(execution_id = %caller.execution_id, ?err, "submit_attachment failed to persist");
            proposals::send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(
                    ProposalErrorCode::Internal,
                    format!("failed to store attachment: {err}"),
                ),
            );
        }
    }
}

pub(super) async fn handle_list_attachments(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ListAttachments { run_id } = req else {
        unreachable!()
    };

    let caller = match proposals::attribute_caller(&server_state, &work_db, peer_pid, &run_id) {
        Ok(caller) => caller,
        Err(error) => {
            tracing::warn!(
                run_id = %run_id,
                peer_pid = ?peer_pid,
                code = %error.code,
                "list_attachments rejected: attribution failed",
            );
            return proposals::send_rejection(&sink, &request_id, error);
        }
    };

    match work_db.list_work_attachments_for_work_item(&caller.work_item_id) {
        Ok(attachments) => send_response(
            &sink,
            &request_id,
            FrontendEvent::AttachmentsList {
                work_item_id: caller.work_item_id,
                attachments,
                evidence_base_url: server_state.evidence_base_url(),
            },
        ),
        Err(err) => {
            tracing::error!(work_item_id = %caller.work_item_id, ?err, "list_attachments failed to read");
            proposals::send_rejection(
                &sink,
                &request_id,
                ProposalSubmissionError::new(
                    ProposalErrorCode::Internal,
                    format!("failed to list attachments: {err}"),
                ),
            );
        }
    }
}

/// App-facing counterpart of [`handle_list_attachments`]: reads every
/// attachment for a caller-named `work_item_id` rather than resolving the
/// scope from the socket peer. The app is not a registered worker run, so
/// `attribute_caller` would fail `AttributionUnresolved` here — this verb
/// exists precisely because that path doesn't work for it, mirroring
/// `handle_list_executions`.
pub(super) async fn handle_list_attachments_for_work_item(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListAttachmentsForWorkItem {
        work_item_id,
        include_revision_chain,
    } = req
    else {
        unreachable!()
    };
    let work_item_id = match server_state.resolve_work_item_id(&work_item_id).await {
        Ok(id) => id,
        Err(err) => {
            send_work_error(&sink, &request_id, &err);
            return;
        }
    };
    let result = if include_revision_chain {
        work_db.list_work_attachments_for_chain(&work_item_id)
    } else {
        work_db.list_work_attachments_for_work_item(&work_item_id)
    };
    match result {
        Ok(attachments) => send_response(
            &sink,
            &request_id,
            FrontendEvent::AttachmentsList {
                work_item_id,
                attachments,
                evidence_base_url: server_state.evidence_base_url(),
            },
        ),
        Err(err) => {
            tracing::error!(
                work_item_id = %work_item_id,
                ?err,
                "list_attachments_for_work_item failed to read",
            );
            send_work_error(&sink, &request_id, &err);
        }
    }
}
