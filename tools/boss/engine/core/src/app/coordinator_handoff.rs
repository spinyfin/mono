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
    // A coordinator-ancestry read doubles as the on-demand credential path:
    // the response says whether the main session's inbox accepted the
    // redelivery (or which step failed), never the credential itself.
    let authorization_delivery = if coordinator_peer(&server_state, peer_pid) {
        Some(deliver_authorization(server_state.clone(), "handoff_show").await)
    } else {
        None
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
                FrontendEvent::CoordinatorHandoffResult {
                    handoff: None,
                    authorization_delivery,
                },
            );
        }
        HandoffState::Present(handoff) => {
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            send_response(
                &sink,
                &request_id,
                FrontendEvent::CoordinatorHandoffResult {
                    handoff: Some(handoff_view(&handoff, current.as_deref(), now)),
                    authorization_delivery,
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

/// The `[Boss handoff authorization]` inbox message. Test sources parse the
/// write command out of this text, so keep its shape stable.
pub(crate) fn authorization_prompt(authorization: &str) -> String {
    format!(
        "[Boss handoff authorization] Use `boss handoff write --authorization {authorization} -` for rolling handoff writes. Keep this credential in the main session only; never delegate or archive it."
    )
}

/// How long a delivery whose confirmation is still outstanding suppresses a
/// repeat request for the same session. Past this, a lost, held or denied
/// frame is retried instead of being reported as pending.
const PENDING_COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// A credential delivery awaiting confirmation, keyed to the coordinator
/// generation it targets so a replacement session is never suppressed by its
/// predecessor's outstanding attempt.
#[derive(Debug, Clone)]
pub(super) struct PendingAuthorizationDelivery {
    spawn_token: String,
    attempt_id: u64,
    started: std::time::Instant,
}

static NEXT_DELIVERY_ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Clear the pending entry only if it is still the given attempt, so a stale
/// confirmation cannot erase a newer attempt's state.
fn clear_pending(pending: &std::sync::Mutex<Option<PendingAuthorizationDelivery>>, attempt_id: u64) {
    if let Ok(mut pending) = pending.lock()
        && pending.as_ref().is_some_and(|entry| entry.attempt_id == attempt_id)
    {
        *pending = None;
    }
}

/// Deliver the live coordinator's handoff write credential to its main
/// session inbox, minting one first when the session predates authorization
/// (it outlived the deploy, or an engine restart adopted it). Never returns
/// the credential: a child can request recovery, but only the main
/// coordinator's inbox receives the secret.
///
/// Resolves as soon as the inbox has accepted the message, so a coordinator
/// that asked mid-turn gets an answer now and the message at its next input
/// boundary. Confirmation continues in the background, bounded by
/// [`crate::coordinator_tmux::inbox::AUTHORIZATION_CONFIRMATION_TIMEOUT`],
/// and records the delivery against the spawn token once confirmed. Every
/// outcome is audited as `coordinator_handoff_authorization_delivery`.
pub(super) async fn deliver_authorization(
    state: Arc<ServerState>,
    trigger: &'static str,
) -> boss_protocol::CoordinatorHandoffAuthorizationDelivery {
    use std::sync::atomic::Ordering;
    let pending = state.coordinator_authorization_delivery_pending.clone();
    let attempt_id = NEXT_DELIVERY_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let accepted: anyhow::Result<Option<(String, crate::coordinator_tmux::inbox::PendingConfirmation)>> = async {
        let program = match &*state
            .tmux_preflight
            .read()
            .map_err(|_| anyhow::anyhow!("tmux preflight lock poisoned"))?
        {
            crate::tmux_preflight::TmuxPreflight::Ready { program, .. }
            | crate::tmux_preflight::TmuxPreflight::ReadyUnknown { program } => program.clone(),
            crate::tmux_preflight::TmuxPreflight::Unavailable { reason } => {
                anyhow::bail!("tmux is unavailable: {reason}")
            }
        };
        let tmux = state.tmux_from_program(program)?;
        let legacy = boss_tmux::Tmux::for_legacy_label_server(tmux.program().to_path_buf()).ok();
        let guard = state.coordinator_tmux_lock.lock().await;
        let tmux = crate::coordinator_tmux::resolve_active_handle(&tmux, legacy.as_ref()).await;
        let record = state
            .work_db
            .coordinator_tmux_record()?
            .ok_or_else(|| anyhow::anyhow!("no coordinator session is on record"))?;
        {
            let mut pending = pending
                .lock()
                .map_err(|_| anyhow::anyhow!("credential delivery state lock poisoned"))?;
            if pending.as_ref().is_some_and(|entry| {
                entry.spawn_token == record.spawn_token && entry.started.elapsed() < PENDING_COALESCE_WINDOW
            }) {
                return Ok(None);
            }
            *pending = Some(PendingAuthorizationDelivery {
                spawn_token: record.spawn_token.clone(),
                attempt_id,
                started: std::time::Instant::now(),
            });
        }
        let authorization = state
            .work_db
            .ensure_coordinator_handoff_authorization(&record.spawn_token)
            .context("minting the handoff credential")?;
        let confirmation = state
            .coordinator_handoff_delivery
            .deliver_accepted(tmux, &record, &authorization_prompt(&authorization), guard)
            .await
            .context("delivering the credential to the coordinator's inbox")?;
        Ok(Some((record.spawn_token, confirmation)))
    }
    .await;
    match accepted {
        Ok(None) => boss_protocol::CoordinatorHandoffAuthorizationDelivery {
            requested: true,
            detail: "a delivery of the handoff credential to the main session's inbox is already pending; it is read at that session's next input boundary".to_owned(),
        },
        Err(error) => {
            clear_pending(&pending, attempt_id);
            let error = format!("{error:#}");
            crate::audit::record_event(
                "coordinator_handoff_authorization_delivery",
                &serde_json::json!({ "trigger": trigger, "outcome": "failed", "error": error }),
            );
            tracing::error!(%error, trigger, "could not deliver the coordinator handoff credential");
            boss_protocol::CoordinatorHandoffAuthorizationDelivery {
                requested: false,
                detail: format!("the handoff credential was not delivered: {error}"),
            }
        }
        Ok(Some((spawn_token, confirmation))) => {
            crate::audit::record_event(
                "coordinator_handoff_authorization_delivery",
                &serde_json::json!({ "trigger": trigger, "outcome": "accepted", "spawn_token": spawn_token }),
            );
            let work_db = state.work_db.clone();
            boss_task_scope::spawn(async move {
                let outcome = tokio::time::timeout(
                    crate::coordinator_tmux::inbox::AUTHORIZATION_CONFIRMATION_TIMEOUT,
                    confirmation,
                )
                .await;
                clear_pending(&pending, attempt_id);
                let (outcome, error) = match outcome {
                    Ok(Ok(())) => {
                        match work_db.record_coordinator_handoff_authorization_delivered(&spawn_token) {
                            Ok(true) => {}
                            Ok(false) => {
                                tracing::warn!("handoff credential confirmed for a replaced coordinator session")
                            }
                            Err(error) => {
                                tracing::warn!(error = %format!("{error:#}"), "could not record handoff credential delivery")
                            }
                        }
                        ("confirmed", None)
                    }
                    Ok(Err(error)) => ("failed", Some(format!("{error:#}"))),
                    Err(_) => (
                        "unconfirmed",
                        Some(format!(
                            "no confirmation within {}s",
                            crate::coordinator_tmux::inbox::AUTHORIZATION_CONFIRMATION_TIMEOUT.as_secs()
                        )),
                    ),
                };
                crate::audit::record_event(
                    "coordinator_handoff_authorization_delivery",
                    &serde_json::json!({ "trigger": trigger, "outcome": outcome, "spawn_token": spawn_token, "error": error }),
                );
                if let Some(error) = error {
                    tracing::error!(%error, trigger, "coordinator handoff credential delivery was not confirmed");
                }
            });
            boss_protocol::CoordinatorHandoffAuthorizationDelivery {
                requested: true,
                detail: "the handoff credential was written to the main session's inbox socket; it is read as a [Boss handoff authorization] message at that session's next input boundary (the end of the current turn)".to_owned(),
            }
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
