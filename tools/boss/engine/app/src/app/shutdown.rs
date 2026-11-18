//! Workers outlive the engine; shutdown must preserve their process ownership.

use super::*;

impl ServerState {
    /// Leave tmux workers intact for startup adoption. Historical local workers
    /// without identity must also survive so an operator can roll back or drain
    /// them; shutdown cannot establish ownership merely from a recorded PID.
    pub async fn shutdown_workers(self: &Arc<Self>) {
        tracing::info!(
            count = self.live_worker_states.snapshot().len(),
            "shutdown_workers: preserving workers for adoption or historical-worker rollback/drain",
        );
    }
}
