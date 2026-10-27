//! `FrontendRequest` handlers — live-status enable/disable and pipeline debug.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_set_live_status_enabled(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::SetLiveStatusEnabled { slot_id, enabled } = req else {
        unreachable!()
    };
    {
        server_state.live_status_manager.set_enabled(slot_id, enabled);
        if let Err(err) =
            persist_live_status_disabled_slots(&work_db, &server_state.live_status_manager.disabled_snapshot())
        {
            tracing::warn!(
                slot_id,
                enabled,
                ?err,
                "live_status: failed to persist disabled-slot toggle",
            );
        }
        send_response(
            &sink,
            &request_id,
            FrontendEvent::LiveStatusEnabledSet { slot_id, enabled },
        );
    }
}

pub(super) async fn handle_list_live_status_disabled_slots(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListLiveStatusDisabledSlots = req else {
        unreachable!()
    };
    {
        let slot_ids = server_state.live_status_manager.disabled_snapshot();
        send_response(
            &sink,
            &request_id,
            FrontendEvent::LiveStatusDisabledSlotsList { slot_ids },
        );
    }
}

pub(super) async fn handle_debug_live_status_pipeline(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::DebugLiveStatusPipeline = req else {
        unreachable!()
    };
    {
        let report = build_live_status_debug_report(&server_state, &work_db);
        send_response(&sink, &request_id, FrontendEvent::LiveStatusDebugReportEvent { report });
    }
}

#[async_trait]
impl TranscriptPathResolver for ServerState {
    async fn worker_can_refresh(&self, run_id: &str) -> bool {
        use crate::stale_worker_sweep::{TerminalLiveness, TmuxWorkerTerminalInspector, WorkerTerminalInspector};
        let execution = self.work_db.get_execution(run_id).ok();
        if execution
            .as_ref()
            .is_some_and(|execution| execution.status.is_terminal())
        {
            self.live_worker_states.mark_terminated(run_id);
            self.broadcast_live_worker_states().await;
            return false;
        }
        if let Ok(tmux) = self.resolve_tmux() {
            let legacy = self.tmux_for_run(&tmux, boss_tmux::SERVER_LABEL).ok();
            let inspector = TmuxWorkerTerminalInspector::new(self.work_db.clone(), tmux, legacy);
            match inspector.inspect(run_id).await {
                Ok(Some(TerminalLiveness::Dead { .. })) => {
                    self.live_worker_states.mark_terminated(run_id);
                    self.broadcast_live_worker_states().await;
                    return false;
                }
                // A live, token-matched pane outweighs a stale shell PID.
                Ok(Some(TerminalLiveness::Alive { .. })) => return true,
                Ok(None) | Err(_) => {}
            }
        }
        let process = crate::durable_liveness::probe_execution_worker(&self.work_db, run_id);
        let process = match execution.and_then(|execution| execution.started_epoch()) {
            Some(started) => {
                crate::durable_liveness::corroborate_against_live_registry(
                    process,
                    &self.live_worker_states,
                    run_id,
                    started,
                    boss_engine_utils::epoch_time::now_epoch_secs(),
                )
                .0
            }
            None => crate::durable_liveness::WorkerProcess::Unknown,
        };
        if matches!(process, crate::durable_liveness::WorkerProcess::Gone { .. }) {
            self.live_worker_states.mark_terminated(run_id);
            self.broadcast_live_worker_states().await;
            return false;
        }
        true
    }

    async fn transcript_path(&self, run_id: &str) -> Option<std::path::PathBuf> {
        // The "run_id" the live-status manager hands us is actually the
        // execution id (`exec_*`) — `LiveWorkerState.run_id` is stamped
        // from `WorkItemBinding.execution_id` at spawn, and the rest of
        // the engine is consistent with that aliasing. The pre-fix
        // version of this resolver called `work_db.get_run(run_id)`
        // (which joins on `work_runs.id`, an `run_*` namespace), so the
        // lookup never matched and the per-slot summarizer never
        // resolved a transcript path. That blocked `tail` from ever
        // being instantiated, which in turn meant `snap.transcript_path`
        // was never populated in the debug store — visible to the user
        // as `bossctl live-status debug --json` reporting
        // `slots[*].transcript_path: null` for every live slot.
        //
        // PR #384 fixed the same cross-namespace bug on the write side
        // (`set_run_transcript_path_if_unset`). This is the read-side
        // pair. Keep both routed through helpers that explicitly take
        // an execution id so a future grep for `work_db.get_run` in this
        // file can stay a strong "this is the wrong namespace" signal.
        match self.work_db.transcript_path_for_execution(run_id) {
            Ok(Some(path)) => Some(std::path::PathBuf::from(path)),
            Ok(None) => None,
            Err(err) => {
                tracing::debug!(run_id, ?err, "live_status: transcript path lookup failed");
                None
            }
        }
    }
}
