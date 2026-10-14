//! Startup wiring for the periodic PR-recovery sweeps, split out of
//! `server.rs` so that file stays under the 3000-line size check.
//!
//! Both sweeps reconcile a work item's PR binding against GitHub from the
//! durable pointer the engine itself created — the execution's branch — and
//! both fire once on boot before settling into their slow intervals, so
//! state left behind by an older engine (or by a GitHub outage while this
//! one was down) is repaired without waiting.

use super::*;

/// Spawn the abandoned-branch PR reconciler and the PR-binding repair sweep.
/// Returns their join handles; the caller keeps them alive for the engine's
/// lifetime like every other sweep handle.
pub(super) fn spawn_pr_recovery_sweeps(
    server_state: &Arc<ServerState>,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    // Abandoned-branch-PR reconciler: catches a terminated worker that
    // pushed its engine-supplied branch but never got a PR opened for it
    // (e.g. a GitHub outage on the PR-create path, or a worker that died
    // before reaching that step). Detects the state directly off the
    // execution row — terminal, workspace-backed, no `pr_url` anywhere —
    // independent of whatever kanban status the task fell back to, then
    // binds an already-existing PR or auto-creates one. Runs on a slow
    // fixed interval; see that module for the full incident writeup and
    // retry/backoff rationale.
    let abandoned_branch = crate::abandoned_branch_pr_sweep::spawn_loop(
        server_state.work_db.clone(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.clone(),
        crate::abandoned_branch_pr_sweep::DEFAULT_INTERVAL,
    );

    // PR-binding repair: a row bound to a PR its own execution branch did
    // not open (its origin PR, or another work item's PR — the 2026-10
    // post-merge follow-up mis-binds), or whose doc pointer was copied off
    // such a PR, is re-bound from the execution's branch and put into
    // review, or parked visibly when no PR exists on that branch.
    let binding_repair = crate::pr_binding_repair_sweep::spawn_loop(
        server_state.work_db.clone(),
        server_state.execution_coordinator.clone(),
        crate::pr_binding_repair_sweep::DEFAULT_INTERVAL,
    );

    (abandoned_branch, binding_repair)
}
