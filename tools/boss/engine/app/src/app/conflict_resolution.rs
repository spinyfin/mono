//! `FrontendRequest` handlers — merge-conflict resolution attempts.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_list_conflict_resolutions(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListConflictResolutions {
        product_id,
        status,
        work_item_id,
        limit,
    } = req
    else {
        unreachable!()
    };
    // Mirror of `ListCiRemediations`: a bare short id must resolve to a
    // canonical id before it reaches the DB filter, or it silently
    // matches nothing (or, worse, a coincidentally-numbered wrong row).
    let work_item_id = match work_item_id {
        Some(id) => match server_state.resolve_work_item_id(&id).await {
            Ok(id) => Some(id),
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
                return;
            }
        },
        None => None,
    };
    {
        // Read-only listing surface for `boss engine conflicts
        // list`. No auth gate — the rows are diagnostic and the
        // caller can already read the SQLite file.
        match work_db.list_conflict_resolutions(product_id.as_deref(), &status, work_item_id.as_deref(), limit) {
            Ok(attempts) => send_response(&sink, &request_id, FrontendEvent::ConflictResolutionsList { attempts }),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_get_conflict_resolution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetConflictResolution { attempt_id } = req else {
        unreachable!()
    };
    {
        match work_db.get_conflict_resolution(&attempt_id) {
            Ok(Some(attempt)) => send_response(&sink, &request_id, FrontendEvent::ConflictResolution { attempt }),
            Ok(None) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: format!("conflict resolution attempt {attempt_id:?} is unknown",),
                },
            ),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_get_conflict_hotspots(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetConflictHotspots { product_id, top } = req else {
        unreachable!()
    };
    {
        // Read-only aggregation surface for `boss engine conflicts
        // hotspots`. No auth gate, same reasoning as
        // `list_conflict_resolutions`: diagnostic-only, and the
        // caller can already read the SQLite file directly.
        let top_n = top.unwrap_or(20) as usize;
        match work_db.conflict_hotspots(&product_id, top_n) {
            Ok(report) => send_response(&sink, &request_id, FrontendEvent::ConflictHotspots { report }),
            Err(err) => send_work_error(&sink, &request_id, &err),
        }
    }
}

pub(super) async fn handle_retry_conflict_resolution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RetryConflictResolution { attempt_id } = req else {
        unreachable!()
    };
    {
        match work_db.retry_conflict_resolution(&attempt_id) {
            Ok(Some(attempt)) => {
                if let Some(reason) = attempt.deferral_end_reason.as_deref() {
                    // The row recorded a deferred conflict, so there was no
                    // attempt to reset and nothing has started yet: the
                    // request released the conflict, and the merge poller
                    // starts the resolution (and announces it) on its next
                    // look at the PR. Ask for that look now, and have the
                    // board re-read the card.
                    tracing::info!(
                        deferral_id = %attempt.id,
                        work_item_id = %attempt.work_item_id,
                        pr_url = %attempt.pr_url,
                        reason,
                        "retry_conflict_resolution: deferred conflict released for resolution",
                    );
                    server_state
                        .publisher
                        .publish_work_item_changed(
                            &attempt.product_id,
                            &attempt.work_item_id,
                            "conflict_deferral_ended",
                        )
                        .await;
                    server_state.event_bus.publish(Event::PrReconcileRequested {
                        pr_url: attempt.pr_url.clone(),
                    });
                } else {
                    tracing::warn!(
                        attempt_id = %attempt.id,
                        work_item_id = %attempt.work_item_id,
                        pr_url = %attempt.pr_url,
                        "retry_conflict_resolution: attempt reset to pending",
                    );
                    // Mirror the freshly-pending start so the macOS
                    // app's activity feed shows the retry as a new
                    // attempt. The wire shape is identical to the
                    // detection-path's started event — the consumer
                    // doesn't need to distinguish.
                    server_state
                        .publisher
                        .publish_frontend_event_on_product(
                            &attempt.product_id,
                            FrontendEvent::ConflictResolutionStarted {
                                attempt: work_db.remediation_entry(attempt.list_entry()),
                                product_id: attempt.product_id.clone(),
                                work_item_id: attempt.work_item_id.clone(),
                                attempt_id: attempt.id.clone(),
                                pr_url: attempt.pr_url.clone(),
                            },
                        )
                        .await;
                }
                send_response(&sink, &request_id, FrontendEvent::ConflictResolutionRetried { attempt });
            }
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "conflict resolution attempt {attempt_id:?} is unknown or not in a terminal-failure state (only failed/abandoned rows can be retried; a deferred row can be retried to resolve it now)",
                        ),
                    },
                );
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

pub(super) async fn handle_abandon_conflict_resolution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::AbandonConflictResolution { attempt_id, reason } = req else {
        unreachable!()
    };
    {
        match work_db.mark_conflict_resolution_abandoned(&attempt_id, &reason) {
            Ok(Some(attempt)) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    pr_url = %attempt.pr_url,
                    %reason,
                    "abandon_conflict_resolution: attempt flipped to abandoned",
                );
                server_state
                    .publisher
                    .publish_frontend_event_on_product(
                        &attempt.product_id,
                        FrontendEvent::ConflictResolutionAbandoned {
                            attempt: work_db.remediation_entry(attempt.list_entry()),
                            product_id: attempt.product_id.clone(),
                            work_item_id: attempt.work_item_id.clone(),
                            attempt_id: attempt.id.clone(),
                            pr_url: attempt.pr_url.clone(),
                            failure_reason: reason.clone(),
                        },
                    )
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ConflictResolutionMarkedAbandoned { attempt },
                );
            }
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("conflict resolution attempt {attempt_id:?} is unknown or already terminal",),
                    },
                );
            }
            Err(err) => {
                send_work_error(&sink, &request_id, &err);
            }
        }
    }
}

pub(super) async fn handle_mark_conflict_resolution_failed(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::MarkConflictResolutionFailed {
        attempt_id,
        reason,
        evidence,
    } = req
    else {
        unreachable!()
    };
    match run_writes::conflict_mark_failed(&server_state, &attempt_id, &reason, evidence.as_ref(), None).await {
        Ok(run_writes::ConflictMarked::ArchitecturalMismatch(attempt)) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::ConflictResolution { attempt: *attempt },
            );
        }
        Ok(run_writes::ConflictMarked::Failed(attempt)) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::ConflictResolutionMarkedFailed { attempt: *attempt },
            );
        }
        Err(err) => run_writes::send_write_error(&sink, &request_id, err),
    }
}

pub(super) async fn handle_record_producer_side_conflict(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RecordProducerSideConflict {
        execution_id,
        head_branch,
        base_branch,
        conflicted_files,
    } = req
    else {
        unreachable!()
    };
    // Worker-facing telemetry surface. `User` tier, like
    // `mark_conflict_resolution_failed`: the only state change is a fresh,
    // self-contained `conflict_resolutions` row.
    let input = ProducerConflictInsertInput::builder()
        .execution_id(execution_id)
        .head_branch(head_branch)
        .base_branch(base_branch)
        .conflicted_files(conflicted_files)
        .build();
    match run_writes::producer_conflict(&server_state, input) {
        Ok(attempt) => send_response(&sink, &request_id, FrontendEvent::ConflictResolution { attempt }),
        Err(err) => run_writes::send_write_error(&sink, &request_id, err),
    }
}
