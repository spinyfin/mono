//! Engine-shutdown worker teardown.
//!
//! Split out of `app.rs` so that file stays under the line budget. The
//! load-bearing behaviour: app-hosted panes are released and their shells
//! signalled; tmux-hosted sessions are left detached so boot-time adoption
//! can re-attach them.

use super::*;

impl ServerState {
    /// Release every live **app-hosted** worker pane the engine knows about.
    /// Called from the engine-shutdown path: walks
    /// `LiveWorkerStateRegistry::snapshot()` and dispatches
    /// [`ServerState::release_worker_pane`] for each app-hosted `run_id` in
    /// parallel.
    ///
    /// App-hosted workers are children of the libghostty surface — once the
    /// pane is released the worker shell exits and `claude` exits with it.
    /// After the bounded join we send a best-effort `SIGTERM` (then
    /// `SIGKILL` after `kill_grace`) to every recorded `shell_pid > 0` of
    /// those workers, covering the case where the app is gone or didn't ack
    /// in time and the shell would otherwise be reparented to launchd.
    ///
    /// Tmux-hosted workers (`work_runs.tmux_hosted = 1`, or the matching
    /// in-memory stamps) are left intact. Their detached sessions outlive
    /// this process so the boot-time adoption pass can re-attach them.
    /// Calling [`Self::release_worker_pane`] here would run `reap_tmux_worker`
    /// (destroy the session and null `tmux_session_name`) and signalling the
    /// shell would kill the process the session exists to keep. Identity
    /// columns stay populated so the adoption predicate still matches.
    ///
    /// `total_timeout` bounds the app-hosted walk. Each individual
    /// `release_worker_pane` call already has its own round-trip budget
    /// against the app, but on shutdown we'd rather forcibly move on than
    /// block the engine exit on an unresponsive app.
    pub async fn shutdown_workers(self: &Arc<Self>, total_timeout: Duration, kill_grace: Duration) {
        let snapshot = self.live_worker_states.snapshot();
        if snapshot.is_empty() {
            tracing::info!("shutdown_workers: no live workers to release");
            return;
        }
        let app_hosted: Vec<_> = snapshot
            .iter()
            .filter(|state| !self.tmux_hosted_worker_survives_shutdown(&state.run_id, state.tmux_hosted))
            .cloned()
            .collect();
        let tmux_hosted = snapshot.len() - app_hosted.len();
        if tmux_hosted > 0 {
            tracing::info!(
                count = tmux_hosted,
                "shutdown_workers: leaving tmux-hosted workers detached for re-adoption",
            );
        }
        if app_hosted.is_empty() {
            return;
        }
        tracing::info!(
            count = app_hosted.len(),
            "shutdown_workers: releasing live app-hosted worker panes",
        );
        let mut set = tokio::task::JoinSet::new();
        for state in &app_hosted {
            let server = Arc::clone(self);
            let run_id = state.run_id.clone();
            set.spawn(async move {
                server.release_worker_pane(&run_id).await;
            });
        }
        let join_all = async { while set.join_next().await.is_some() {} };
        if tokio::time::timeout(total_timeout, join_all).await.is_err() {
            tracing::warn!(
                timeout_secs = total_timeout.as_secs(),
                "shutdown_workers: release timed out; falling back to direct kill",
            );
        }
        let pids: Vec<libc::pid_t> = app_hosted
            .iter()
            .filter_map(|s| (s.shell_pid > 0).then_some(s.shell_pid as libc::pid_t))
            .collect();
        signal_shell_pids(&pids, kill_grace);
    }

    /// True when this live worker was dispatched onto the tmux-hosting path
    /// and must outlive this engine process. Consults the live-state stamp,
    /// the worker-registry pane, and the durable `tmux_hosted` column — any
    /// one of them is enough, so a missing in-memory stamp cannot
    /// accidentally reap a surviving session.
    fn tmux_hosted_worker_survives_shutdown(&self, run_id: &str, live_tmux_hosted: Option<bool>) -> bool {
        live_tmux_hosted == Some(true)
            || self
                .worker_registry
                .pane_for_run(run_id)
                .is_some_and(|pane| pane.tmux_hosted)
            || matches!(
                self.work_db.latest_run_tmux_hosting_for_execution(run_id),
                Ok(Some(true))
            )
    }
}
