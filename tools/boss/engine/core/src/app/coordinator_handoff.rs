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
//! Writes require a session credential and coordinator process ancestry.
//! Worker denial applies even with the general worker-tier flag disabled.

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

fn coordinator_peer(state: &ServerState, peer: Option<libc::pid_t>) -> bool {
    !state.classify_peer(peer).is_worker()
        && matches!((peer, state.current_boss_pid()), (Some(peer), Some(root)) if is_descendant_of_any(peer, &[root]))
}

pub(super) async fn handle_get_coordinator_handoff(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::GetCoordinatorHandoff = req else {
        unreachable!()
    };
    if !coordinator_peer(&server_state, peer_pid) {
        let expected = work_db.coordinator_handoff_authorization().ok().flatten();
        crate::audit::record_event(
            "coordinator_handoff_rejected",
            &serde_json::json!({
                "pid": peer_pid,
                "caller": rejected_caller(peer_pid, &[expected.as_deref().unwrap_or_default()]),
                "reason": "handoff read requires coordinator ancestry; workers are forbidden",
            }),
        );
        return send_work_error(
            &sink,
            &request_id,
            "coordinator handoff requires live coordinator process ancestry and is unavailable to workers".to_owned(),
        );
    }
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
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::SetCoordinatorHandoff { body, authorization } = req else {
        unreachable!()
    };
    // Pin authorization and attribution to one coordinator generation.
    let _lifecycle = server_state.coordinator_tmux_lock.lock().await;
    let expected = work_db.coordinator_handoff_authorization().ok().flatten();
    let coordinator_pid = server_state.current_boss_pid();
    let permitted = coordinator_peer(&server_state, peer_pid)
        && expected.as_ref().is_some_and(|expected| {
            !expected.is_empty() && constant_time_eq(expected.as_bytes(), authorization.as_bytes())
        });
    if !permitted {
        let caller = rejected_caller(
            peer_pid,
            &[authorization.as_str(), expected.as_deref().unwrap_or_default()],
        );
        crate::audit::record_event(
            "coordinator_handoff_rejected",
            &serde_json::json!({
                "pid": peer_pid,
                "coordinator_pid": coordinator_pid,
                "caller": caller,
                "reason": "requires coordinator ancestry and the private session credential; workers are forbidden",
            }),
        );
        return send_work_error(&sink, &request_id,
            "handoff write requires the live coordinator and its private session authorization; inherited BOSS_SPAWN_TOKEN grants no access. Restart a coordinator launched before handoff authorization was installed.".to_owned());
    }
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
    if let HandoffState::Present(previous) = work_db.coordinator_handoff_state()
        && body.len().saturating_mul(5) < previous.body.len()
    {
        crate::audit::record_event(
            "coordinator_handoff_shrink_warning",
            &serde_json::json!({
                "previous_bytes": previous.body.len(), "bytes": body.len(), "pid": peer_pid,
                "writer_spawn_token": writer,
            }),
        );
        tracing::warn!(
            previous_bytes = previous.body.len(),
            bytes = body.len(),
            "coordinator handoff shrank by over 80 percent"
        );
    }
    match work_db.set_coordinator_handoff(&body, writer, now) {
        Ok(handoff) => {
            server_state.coordinator_handoff_written.notify_waiters();
            crate::audit::record_event(
                "coordinator_handoff_written",
                &serde_json::json!({
                    "bytes": handoff.body.len(),
                    "pid": peer_pid,
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

/// Best-effort diagnostics only: authorization never trusts argv or environment.
/// Bound the ancestry walk; redact the presented and real secrets
/// even when they occur inside an ancestor shell's command string.
pub(super) fn rejected_caller(peer: Option<libc::pid_t>, secrets: &[&str]) -> serde_json::Value {
    let mut chain = Vec::new();
    let mut next = peer;
    while let Some(pid) = next {
        if pid <= 0 || chain.contains(&pid) || chain.len() == 16 {
            break;
        }
        chain.push(pid);
        next = crate::worker_registry::parent_pid(pid).ok().flatten();
    }
    let mut argv = chain
        .iter()
        .map(|pid| {
            let command = boss_engine_utils::process_args::command(*pid as u32)
                .unwrap_or_else(|| "[argv unavailable]".to_owned());
            format!("{pid}: {command}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        argv = argv.replace(secret, "[redacted]");
    }
    serde_json::json!({"parent_chain": chain, "argv": argv})
}
