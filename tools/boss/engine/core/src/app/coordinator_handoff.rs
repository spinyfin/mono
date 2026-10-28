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
    !state.classify_peer(peer).is_agent()
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
    if server_state.classify_peer(peer_pid).is_agent() {
        let expected = work_db.coordinator_handoff_authorization().ok().flatten();
        crate::audit::record_event(
            "coordinator_handoff_rejected",
            &serde_json::json!({
                "pid": peer_pid,
                "caller": rejected_caller(peer_pid, expected.as_deref().unwrap_or_default(), ""),
                "reason": "handoff reads are unavailable to agents",
            }),
        );
        return send_work_error(
            &sink,
            &request_id,
            "coordinator handoff is unavailable to agents".to_owned(),
        );
    }
    if coordinator_peer(&server_state, peer_pid) {
        redeliver_authorization(server_state.clone());
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

/// Never return the credential to the requesting process: a child can request
/// recovery, but only the main coordinator's inbox receives the secret.
fn redeliver_authorization(state: Arc<ServerState>) {
    boss_task_scope::spawn(async move {
        let result: anyhow::Result<()> = async {
            let program = match &*state.tmux_preflight.read().map_err(|_| anyhow::anyhow!("tmux preflight lock poisoned"))? {
                crate::tmux_preflight::TmuxPreflight::Ready { program, .. } => program.clone(),
                crate::tmux_preflight::TmuxPreflight::Unavailable { reason } => anyhow::bail!("{reason}"),
            };
            let tmux = state.tmux_from_program(program)?;
            let legacy = boss_tmux::Tmux::for_legacy_label_server(tmux.program().to_path_buf()).ok();
            let guard = state.coordinator_tmux_lock.lock().await;
            let tmux = crate::coordinator_tmux::resolve_active_handle(&tmux, legacy.as_ref()).await;
            let record = state.work_db.coordinator_tmux_record()?.ok_or_else(|| anyhow::anyhow!("no coordinator session"))?;
            let authorization = state.work_db.ensure_coordinator_handoff_authorization(&record.spawn_token)?;
            let prompt = format!("[Boss handoff authorization] Use `boss handoff write --authorization {authorization} -` for rolling handoff writes. Keep this credential in the main session only; never delegate or archive it.");
            drop(state.coordinator_handoff_delivery.deliver_locked(tmux, &record, &prompt, guard).await?);
            Ok(())
        }.await;
        if let Err(error) = result {
            tracing::error!(%error, "could not redeliver coordinator handoff authorization");
        }
    });
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
        let reason = if coordinator_pid.is_none() {
            "coordinator pid unknown"
        } else {
            "requires coordinator ancestry and the private session credential; agents are forbidden"
        };
        let caller = rejected_caller(
            peer_pid,
            expected.as_deref().unwrap_or_default(),
            authorization.as_str(),
        );
        crate::audit::record_event(
            "coordinator_handoff_rejected",
            &serde_json::json!({
                "pid": peer_pid,
                "coordinator_pid": coordinator_pid,
                "caller": caller,
                "reason": reason,
            }),
        );
        return send_work_error(
            &sink,
            &request_id,
            format!("handoff write rejected: {reason}; inherited BOSS_SPAWN_TOKEN grants no access"),
        );
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
/// Bound the ancestry walk. The real expected credential is redacted by
/// substring wherever it appears. The caller-chosen presented value is
/// redacted only as the token after `--authorization` or inside
/// `--authorization=`, so a rejected caller cannot erase the rest of argv
/// by passing `/`, `-`, `boss`, or another short string.
pub(super) fn rejected_caller(peer: Option<libc::pid_t>, expected: &str, presented: &str) -> serde_json::Value {
    let mut chain = Vec::new();
    let mut next = peer;
    while let Some(pid) = next {
        if pid <= 0 || chain.contains(&pid) || chain.len() == 16 {
            break;
        }
        chain.push(pid);
        next = crate::worker_registry::parent_pid(pid).ok().flatten();
    }
    let argv = chain
        .iter()
        .map(|pid| {
            let command = boss_engine_utils::process_args::command(*pid as u32)
                .unwrap_or_else(|| "[argv unavailable]".to_owned());
            format!("{pid}: {command}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::json!({
        "parent_chain": chain,
        "argv": redact_rejected_caller_argv(&argv, expected, presented),
    })
}

pub(super) fn redact_rejected_caller_argv(argv: &str, expected: &str, presented: &str) -> String {
    let mut out = argv.to_owned();
    if !expected.is_empty() {
        out = out.replace(expected, "[redacted]");
    }
    redact_presented_authorization_flag(&out, presented)
}

fn redact_presented_authorization_flag(argv: &str, presented: &str) -> String {
    if presented.is_empty() {
        return argv.to_owned();
    }
    argv.split('\n')
        .map(|line| redact_presented_authorization_in_line(line, presented))
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_presented_authorization_in_line(line: &str, presented: &str) -> String {
    let tokens: Vec<&str> = line.split(' ').collect();
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        if let Some(value) = token.strip_prefix("--authorization=") {
            if value == presented {
                out.push("--authorization=[redacted]".to_owned());
            } else {
                out.push(token.to_owned());
            }
            i += 1;
            continue;
        }
        out.push(token.to_owned());
        i += 1;
        if token == "--authorization" && i < tokens.len() && tokens[i] == presented {
            out.push("[redacted]".to_owned());
            i += 1;
        }
    }
    out.join(" ")
}
