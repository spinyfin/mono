//! The engine side of un-parking: assembling the environment the un-park
//! reconciliation runs against, and handing a finished un-park to it.
//!
//! The sequence itself lives in `boss_engine_runtime::unpark_reconcile`; this
//! module only borrows the services it needs from [`ServerState`].

use boss_engine_runtime::unpark_reconcile::{RegistryGhBudget, UnparkEnv};
use boss_work_db::parking_reconcile::UnparkReport;

use super::*;

impl ServerState {
    /// The services the un-park reconciliation's steps run against, all shared
    /// with the engine's own sweeps: the merge poller's probe and host, the
    /// completion handler, the dispatcher and its event sink.
    pub(super) fn unpark_env(self: &Arc<Self>) -> UnparkEnv {
        UnparkEnv::builder()
            .work_db(self.work_db.clone())
            .probe(self.merge_probe.clone())
            .publisher(self.publisher.clone())
            .cube_client(self.cube_client.clone())
            .completion(self.completion_handler.clone())
            .host(self.sweep_host.clone())
            .coordinator(self.execution_coordinator.clone())
            .dispatch_events(self.dispatch_events.clone())
            .convergence(self.clone())
            .gh_budget(Arc::new(RegistryGhBudget(self.metrics.clone())))
            .max_review_cycles(self.max_review_cycles)
            .build()
    }

    /// Finish an un-park: leave the audit trail for the database steps the
    /// un-park transaction already performed, then schedule the background
    /// steps. Returns once the trail is written; the job runs on its own.
    pub(super) async fn begin_unpark_reconcile(self: &Arc<Self>, actor: &str, target_id: &str, report: UnparkReport) {
        let env = self.unpark_env();
        // The handle is dropped on purpose: the job reports through the audit
        // trail and dispatch events, and nothing waits on it.
        drop(self.unpark_reconciler.begin(&env, actor, target_id, report).await);
    }
}
