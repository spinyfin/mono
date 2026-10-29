//! Live-status frontend handlers and the production transcript-path resolver.
//!
//! The `FrontendRequest` handlers (pipeline debug) are a
//! structural split from `app.rs`. [`TranscriptPathResolver`] for
//! [`ServerState`] additionally consults
//! [`crate::worker_refresh_liveness`]: the same confirmed-death predicate
//! the stale-worker sweep uses, so a live-status refresh, a literal write,
//! and an in-flight model result all apply one liveness standard.
//! See [`super::Dispatch`] for the per-request context every handler receives.

use super::*;

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
    async fn worker_can_refresh(&self, run_id: &str) -> crate::worker_refresh_liveness::WorkerRefreshPermit {
        use crate::stale_worker_sweep::{TmuxWorkerTerminalInspector, WorkerTerminalInspector};
        let inspector = self.resolve_tmux().ok().map(|tmux| {
            let legacy = self.tmux_for_run(&tmux, boss_tmux::SERVER_LABEL).ok();
            TmuxWorkerTerminalInspector::new(self.work_db.clone(), tmux, legacy)
        });
        let outcome = crate::worker_refresh_liveness::evaluate(
            &self.work_db,
            &self.live_worker_states,
            inspector
                .as_ref()
                .map(|inspector| inspector as &dyn WorkerTerminalInspector),
            run_id,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        if outcome.broadcast {
            self.broadcast_live_worker_states().await;
        }
        outcome.permit
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
