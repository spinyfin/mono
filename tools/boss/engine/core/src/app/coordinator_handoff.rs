//! `FrontendRequest::GetCoordinatorHandoff` / `SetCoordinatorHandoff`
//! handlers — `boss handoff show` / `boss handoff write`.
//!
//! The coordinator session handoff is the short brief the outgoing
//! coordinator session writes so the next one does not start with zero
//! knowledge of what the operator said; see [`crate::coordinator_handoff`]
//! for the storage, state, and delivery design. These handlers are the
//! write path (the coordinator itself, via the CLI) and the read-back path
//! (`show`, e.g. after context compaction).
//!
//! Both are coordinator-only at the worker-tier gate
//! (`boss_worker_policy::worker_verb_decision`); nothing here is
//! peer-attributed beyond that.

use super::*;

use crate::coordinator_handoff::{HandoffState, handoff_view, validate_handoff_body};

/// Spawn token of the coordinator record that is live now, for stamping
/// a write and for judging `written_by_current_session` on a read.
fn current_spawn_token(work_db: &WorkDb) -> Result<Option<String>, String> {
    work_db
        .coordinator_tmux_record()
        .map(|record| record.map(|record| record.spawn_token))
        .map_err(|err| format!("failed to read the coordinator record: {err:#}"))
}

pub(super) async fn handle_get_coordinator_handoff(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetCoordinatorHandoff = req else {
        unreachable!()
    };
    let current = match current_spawn_token(&work_db) {
        Ok(current) => current,
        Err(message) => {
            tracing::error!(%message, "get_coordinator_handoff failed");
            return send_work_error(&sink, &request_id, message);
        }
    };
    match work_db.coordinator_handoff_state() {
        HandoffState::Missing => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::CoordinatorHandoffResult { handoff: None },
            );
        }
        HandoffState::Present(handoff) => {
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            send_response(
                &sink,
                &request_id,
                FrontendEvent::CoordinatorHandoffResult {
                    handoff: Some(handoff_view(&handoff, current.as_deref(), now)),
                },
            );
        }
        HandoffState::Unreadable(reason) => {
            // Deliberately an error, not an empty success: "stored but
            // unreadable" must never read as "nothing was handed off".
            tracing::error!(%reason, "get_coordinator_handoff: stored handoff is unreadable");
            send_work_error(&sink, &request_id, reason);
        }
    }
}

pub(super) async fn handle_set_coordinator_handoff(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetCoordinatorHandoff { body } = req else {
        unreachable!()
    };
    let body = match validate_handoff_body(&body) {
        Ok(body) => body,
        Err(message) => {
            tracing::warn!(%message, "set_coordinator_handoff rejected");
            return send_work_error(&sink, &request_id, message);
        }
    };
    let current = match current_spawn_token(&work_db) {
        Ok(current) => current,
        Err(message) => {
            tracing::error!(%message, "set_coordinator_handoff failed");
            return send_work_error(&sink, &request_id, message);
        }
    };
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let writer = current.as_deref().unwrap_or_default();
    match work_db.set_coordinator_handoff(&body, writer, now) {
        Ok(handoff) => {
            crate::audit::record_event(
                "coordinator_handoff_written",
                &serde_json::json!({
                    "bytes": handoff.body.len(),
                    "writer_spawn_token": handoff.writer_spawn_token,
                    "written_at": handoff.written_at,
                }),
            );
            tracing::info!(
                bytes = handoff.body.len(),
                writer_spawn_token = %handoff.writer_spawn_token,
                "coordinator handoff written"
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::CoordinatorHandoffSet {
                    handoff: handoff_view(&handoff, current.as_deref(), now),
                },
            );
        }
        Err(err) => {
            tracing::error!(?err, "set_coordinator_handoff failed to persist");
            send_work_error(
                &sink,
                &request_id,
                format!("failed to persist coordinator handoff: {err:#}"),
            );
        }
    }
}
