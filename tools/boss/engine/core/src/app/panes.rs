//! `FrontendRequest` handlers — worker pane focus/input/interrupt and live states.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_focus_worker_pane(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::FocusWorkerPane { run_id } = req else {
        unreachable!()
    };
    {
        // `bossctl agents focus` is a coordinator verb that
        // raises a sibling worker pane to the front. The
        // human invokes it from wherever they are — boss
        // pane, app shell, or another worker pane — so the
        // tier is `AppOrBoss`, matching `probe_run` /
        // `stop_run` (which are also legal from inside a
        // worker pane).
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "focus_worker_pane rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "focus_worker_pane requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.focus_worker_pane(&run_id).await {
            Ok(slot_id) => {
                tracing::info!(
                    run_id = %run_id,
                    slot_id,
                    "focus_worker_pane: pane raised",
                );
                send_response(&sink, &request_id, FrontendEvent::WorkerPaneFocused { run_id, slot_id });
            }
            Err(err) => {
                tracing::warn!(?err, run_id = %run_id, "focus_worker_pane failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("focus_worker_pane: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_send_input_to_worker(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::SendInputToWorker { run_id, text } = req else {
        unreachable!()
    };
    {
        // `bossctl agents send` writes user-typed input into a
        // sibling worker pane. Same authority story as
        // `focus_worker_pane` / `probe_run` / `stop_run`: the
        // human invokes this from wherever they are (boss
        // pane, app shell, or another worker pane), so the
        // tier is `AppOrBoss` — caller must descend from the
        // app or the Boss session.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "send_input_to_worker rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "send_input_to_worker requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.send_input_to_worker(&run_id, text).await {
            Ok(slot_id) => {
                tracing::info!(
                    run_id = %run_id,
                    slot_id,
                    "send_input_to_worker: text injected",
                );
                send_response(&sink, &request_id, FrontendEvent::WorkerInputSent { run_id, slot_id });
            }
            Err(err) => {
                tracing::warn!(?err, run_id = %run_id, "send_input_to_worker failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("send_input_to_worker: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_interrupt_worker_pane(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::InterruptWorkerPane { run_id } = req else {
        unreachable!()
    };
    {
        // `bossctl agents interrupt` mirrors the keyboard Esc
        // a human would press inside the worker pane. Same
        // tier rationale as `focus_worker_pane`: the human
        // may invoke it from the Boss pane, the app shell,
        // or a sibling worker pane — `AppOrBoss` admits all
        // three.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "interrupt_worker_pane rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "interrupt_worker_pane requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.interrupt_worker_pane(&run_id).await {
            Ok(slot_id) => {
                tracing::info!(
                    run_id = %run_id,
                    slot_id,
                    "interrupt_worker_pane: esc delivered",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkerPaneInterrupted { run_id, slot_id },
                );
            }
            Err(err) => {
                tracing::warn!(?err, run_id = %run_id, "interrupt_worker_pane failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("interrupt_worker_pane: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_list_worker_live_states(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListWorkerLiveStates = req else {
        unreachable!()
    };
    {
        let states = server_state.live_worker_states_snapshot();
        send_response(&sink, &request_id, FrontendEvent::WorkerLiveStatesList { states });
    }
}

pub(super) async fn handle_list_tmux_worker_statuses(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListTmuxWorkerStatuses = req else {
        unreachable!()
    };
    {
        let statuses = server_state.tmux_worker_statuses().await;
        send_response(&sink, &request_id, FrontendEvent::TmuxWorkerStatusesList { statuses });
    }
}

impl ServerState {
    /// Collect the tmux-only half of `agents list` on demand. The hook-driven
    /// live-state feed intentionally does not call tmux: doing so on every
    /// worker event would turn a lightweight broadcast into a process probe.
    pub(super) async fn tmux_worker_statuses(&self) -> Vec<boss_protocol::TmuxWorkerStatus> {
        let states = self.live_worker_states_snapshot();
        let identities = states
            .iter()
            .map(|state| {
                (
                    state.run_id.clone(),
                    self.work_db.tmux_identity_for_execution(&state.run_id),
                )
            })
            .collect::<Vec<_>>();

        let mut statuses = Vec::with_capacity(identities.len());
        for (execution_id, identity) in identities {
            let identity = match identity {
                Ok(Some(identity)) => identity,
                Ok(None) => {
                    statuses.push(not_tmux_hosted_status(execution_id));
                    continue;
                }
                Err(err) => {
                    tracing::warn!(execution_id, error = %format!("{err:#}"), "agents list: failed reading tmux identity");
                    statuses.push(probe_unavailable_status(execution_id, None));
                    continue;
                }
            };
            let session_name = identity.session_name;
            // Resolved per-run (not once for the whole batch): a run's
            // session may live on either the durable socket or the
            // pre-move `-L boss` server, and `tmux_for_pane_delivery`
            // is the shared routing logic that knows which.
            let tmux = match self.tmux_for_pane_delivery(&execution_id) {
                Ok(tmux) => tmux,
                Err(err) => {
                    tracing::debug!(execution_id, error = %format!("{err:#}"), "agents list: tmux evidence unavailable");
                    statuses.push(probe_unavailable_status(execution_id, Some(session_name)));
                    continue;
                }
            };
            let session_exists = match tmux.list_sessions().await {
                Ok(sessions) => sessions.iter().any(|session| session.name == session_name),
                Err(err) => {
                    tracing::debug!(execution_id, error = %format!("{err:#}"), "agents list: tmux session inventory unavailable");
                    statuses.push(probe_unavailable_status(execution_id, Some(session_name)));
                    continue;
                }
            };
            let observation = crate::tmux_adoption::observe_tmux_identity(
                &tmux,
                &session_name,
                &identity.spawn_token,
                session_exists,
            )
            .await;
            let attach_command = (observation.adoption_state == boss_protocol::TmuxAdoptionState::Adopted)
                .then(|| tmux.attach_session_command(&session_name));
            let last_output_at = observation
                .window_activity_epoch_secs
                .filter(|epoch| *epoch > 0)
                .map(crate::live_worker_state::iso8601_utc);
            statuses.push(tmux_status(
                execution_id,
                Some(session_name),
                observation.adoption_state,
                observation.pane_dead,
                last_output_at,
                attach_command,
            ));
        }
        statuses
    }
}

fn tmux_status(
    execution_id: String,
    session_name: Option<String>,
    adoption_state: boss_protocol::TmuxAdoptionState,
    pane_dead: Option<bool>,
    last_output_at: Option<String>,
    attach_command: Option<String>,
) -> boss_protocol::TmuxWorkerStatus {
    boss_protocol::TmuxWorkerStatus::builder()
        .execution_id(execution_id)
        .maybe_session_name(session_name)
        .adoption_state(adoption_state)
        .maybe_pane_dead(pane_dead)
        .maybe_last_output_at(last_output_at)
        .maybe_attach_command(attach_command)
        .build()
}

fn not_tmux_hosted_status(execution_id: String) -> boss_protocol::TmuxWorkerStatus {
    tmux_status(
        execution_id,
        None,
        boss_protocol::TmuxAdoptionState::NotTmuxHosted,
        None,
        None,
        None,
    )
}

fn probe_unavailable_status(execution_id: String, session_name: Option<String>) -> boss_protocol::TmuxWorkerStatus {
    tmux_status(
        execution_id,
        session_name,
        boss_protocol::TmuxAdoptionState::ProbeUnavailable,
        None,
        None,
        None,
    )
}

pub(super) async fn handle_retire_pane(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::RetirePane { slot_id } = req else {
        unreachable!()
    };
    {
        // Break-glass admin action, same tier as `reap`: it must not be
        // reachable from inside a worker pane subtree — a worker
        // should never be able to retire a sibling's slot.
        if !server_state.authorize_rpc(RpcTier::BossOnly, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                slot_id,
                "retire_pane rejected: caller not in Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "retire_pane requires Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.retire_pane(slot_id).await {
            Ok(()) => {
                tracing::info!(slot_id, "retire_pane: pane retired");
                send_response(&sink, &request_id, FrontendEvent::PaneRetired { slot_id });
            }
            Err(err) => {
                tracing::warn!(?err, slot_id, "retire_pane failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("retire_pane: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_open_document(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::OpenDocument { path } = req else {
        unreachable!()
    };
    {
        // `bossctl open` displays a document in the app, same authority
        // tier as `reveal` / `focus_worker_pane` (both are also
        // UI-steering RPCs invoked from the Boss pane or app shell).
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                path = %path,
                "open_document rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "open_document requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.open_document(&path).await {
            Ok(()) => {
                tracing::info!(path = %path, "open_document: document opened");
                send_response(&sink, &request_id, FrontendEvent::DocumentOpened { path });
            }
            Err(err) => {
                tracing::warn!(?err, path = %path, "open_document failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("open_document: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_list_hosted_pane_statuses(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListHostedPaneStatuses = req else {
        unreachable!()
    };
    {
        match server_state.list_hosted_pane_statuses().await {
            Ok(panes) => {
                send_response(&sink, &request_id, FrontendEvent::HostedPaneStatusList { panes });
            }
            Err(err) => {
                tracing::warn!(?err, "list_hosted_pane_statuses failed");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("list_hosted_pane_statuses: {err}"),
                    },
                );
            }
        }
    }
}
