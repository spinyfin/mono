//! Engine wiring for [`crate::tmux_input_watch`]: the periodic pass over the
//! registered app's tmux-hosted panes, and the three places a recovery is
//! made visible afterwards.
//!
//! Lives under `app` rather than beside the detection rule because it reads
//! private [`ServerState`] fields. The rule itself stays free of engine
//! state so it can be exercised without one.

use std::sync::Arc;
use std::time::Instant;

use crate::tmux_input_watch::{MAX_RECOVERIES, TICK, WatchOutcome};

use super::ServerState;
use super::engine_health::raise_engine_health_attention;

/// Wedge-watch loop for the registered app's tmux-hosted panes.
///
/// Deliberately its own task rather than a step inside the coordinator tmux
/// supervisor: that loop's delay stretches to a minute under restart backoff,
/// and a viewer an operator is actively typing into should not wait on it.
pub(crate) async fn run(server_state: Arc<ServerState>) {
    loop {
        tokio::time::sleep(TICK).await;
        // Zero cost while nobody is typing: no report, no tmux call.
        if server_state.pane_input_reports.snapshot().is_empty() {
            continue;
        }
        let program = match server_state.tmux_preflight.read() {
            Ok(guard) => match &*guard {
                crate::tmux_preflight::TmuxPreflight::Ready { program, .. } => program.clone(),
                crate::tmux_preflight::TmuxPreflight::Unavailable { .. } => continue,
            },
            Err(_) => {
                tracing::error!("tmux input watch stopped: preflight lock poisoned");
                return;
            }
        };
        let Ok(tmux) = boss_tmux::Tmux::from_path(program) else {
            tracing::error!("tmux input watch stopped: preflight supplied an invalid path");
            return;
        };
        let outcomes = server_state
            .tmux_input_watch
            .lock()
            .await
            .tick(
                &tmux,
                server_state.pane_input_reports.as_ref(),
                Instant::now(),
                boss_engine_utils::epoch_time::now_epoch_secs(),
            )
            .await;
        for outcome in outcomes {
            report_outcome(&server_state, outcome).await;
        }
    }
}

/// Make one pass's outcome visible three ways: the engine log, the durable
/// `engine-audit.log` (`tools/boss/docs/forensic-surfaces.md`), and the
/// operator's attention feed.
///
/// A self-heal that leaves no trace is worse than the wedge: it hides a live
/// defect behind a pane that briefly blinks. The attention text carries the
/// recovery count so a *repeat* reads differently from a one-off — attention
/// dedup is content-keyed, so a rising count appends a new item to the same
/// group rather than being swallowed as a duplicate.
async fn report_outcome(server_state: &Arc<ServerState>, outcome: WatchOutcome) {
    match outcome {
        WatchOutcome::Recovered {
            session,
            tty,
            client_pid,
            recovery_count,
        } => {
            tracing::warn!(
                %session,
                %tty,
                client_pid,
                recovery_count,
                "tmux viewer accepted no input while its pane produced output; detached the \
                 client so the app rebuilds it (session, pane and process untouched)"
            );
            crate::audit::record_event(
                "tmux_client_input_wedge_recovered",
                &serde_json::json!({
                    "session": session,
                    "tty": tty,
                    "client_pid": client_pid,
                    "recovery_count": recovery_count,
                }),
            );
            let plural = if recovery_count == 1 { "time" } else { "times" };
            raise_engine_health_attention(
                server_state,
                "engine_health_tmux_input_wedge",
                "engine-health/tmux-input-wedge",
                format!(
                    "The {session} pane stopped accepting input while still rendering output. Boss \
                     detached its tmux client and the app reattached; the session, the pane and the \
                     process inside it were not touched. This has happened {recovery_count} {plural} \
                     in the last 10 minutes."
                ),
            )
            .await;
        }
        WatchOutcome::AlreadyGone { session, tty } => {
            tracing::info!(%session, %tty, "tmux input watch: client had already detached");
        }
        WatchOutcome::Escalated { session, client_pid } => {
            tracing::error!(
                %session,
                client_pid,
                "tmux viewer keeps losing its input path; automatic recovery paused after \
                 {MAX_RECOVERIES} attempts"
            );
            crate::audit::record_event(
                "tmux_client_input_wedge_escalated",
                &serde_json::json!({ "session": session, "client_pid": client_pid }),
            );
            raise_engine_health_attention(
                server_state,
                "engine_health_tmux_input_wedge",
                "engine-health/tmux-input-wedge",
                format!(
                    "The {session} pane has lost its input path {MAX_RECOVERIES} times in 10 minutes. \
                     Automatic recovery is paused until the window expires — this is a defect that \
                     needs looking at, not a transient."
                ),
            )
            .await;
        }
        WatchOutcome::Failed { session, tty, error } => {
            tracing::error!(
                %session,
                %tty,
                %error,
                "tmux input watch: detaching the wedged client failed"
            );
        }
    }
}
