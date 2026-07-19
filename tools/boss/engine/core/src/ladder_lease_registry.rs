//! In-memory registry of cube workspace leases currently held by an
//! in-flight [`crate::conflict_ladder`] rung-1 attempt.
//!
//! ## Why this exists
//!
//! A rung-1 attempt (`conflict_ladder::try_mechanical_rungs`) leases a cube
//! workspace, runs an engine-direct rebase, and releases the lease
//! unconditionally — entirely within one in-process async call. No
//! `work_executions` row, no worker pane, no
//! [`crate::live_worker_state::LiveWorkerStateRegistry`] entry backs it, so
//! none of the engine's existing worker-lease machinery
//! ([`crate::cube_lease_heartbeat`], [`crate::dead_pid_sweep`],
//! [`crate::run_reconcile`]) knows it exists. If the engine process exits
//! (crash, restart, shutdown) between acquiring the lease and running its
//! release, the lease is orphaned to the dead engine and sits until cube's
//! TTL sweep reclaims it — up to 30 minutes later. See the 2026-07-18
//! incident: workspace `flunge-agent-035`, lease held by conflict-ladder
//! attempt `crz_18c35db426d18878_7d9`, orphaned across an engine restart
//! that landed 31 seconds after the lease was acquired.
//!
//! This registry closes the gap for the *clean-shutdown* case: it tracks
//! every lease a rung-1 attempt currently holds so
//! [`release_all_on_shutdown`] can release them before the engine exits,
//! mirroring how `ServerState::shutdown_workers` releases live worker
//! panes. The crash / kill-9 case (no shutdown path runs at all) is instead
//! covered at the next engine startup by [`crate::ladder_lease_reap`],
//! which does not depend on this in-memory registry — it is empty on every
//! fresh process, restart included.
//!
//! ## Why a process-wide static instead of a threaded `Arc`
//!
//! `try_mechanical_rungs` sits at the bottom of a call chain
//! (`merge_poller` → `conflict_watch::on_conflict_detected` →
//! `conflict_ladder::try_mechanical_rungs`) whose middle link,
//! `on_conflict_detected`, has upwards of 50 call sites (almost all test
//! fixtures). Threading a new `Arc<...>` registry parameter through that
//! signature would mean updating every one of them for a concern those
//! tests never exercise. [`crate::populator`] hit the identical shape
//! (a hook deep in the poller's call chain with a narrow, already-fixed
//! signature) and settled on a process-wide `OnceLock` for exactly this
//! reason — this module follows the same precedent.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::coordinator::{CubeClient, CubeWorkspaceLease};

static ACTIVE_LEASES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn active_leases() -> &'static Mutex<HashMap<String, String>> {
    ACTIVE_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that `lease` is now held by an in-flight rung-1 attempt. Called
/// right after `CubeClient::lease_workspace` succeeds in
/// `conflict_ladder::try_mechanical_rungs`.
pub fn register(lease: &CubeWorkspaceLease) {
    active_leases()
        .lock()
        .unwrap()
        .insert(lease.lease_id.clone(), lease.workspace_id.clone());
}

/// Forget `lease_id`. Called once the ladder's own unconditional release
/// has run — success or failure, either way the in-process attempt is done
/// with it, so it is no longer this registry's concern.
pub fn unregister(lease_id: &str) {
    active_leases().lock().unwrap().remove(lease_id);
}

/// Snapshot of `(lease_id, workspace_id)` pairs currently tracked. Used by
/// the shutdown release path below and the short-TTL heartbeat sweep
/// (`crate::ladder_lease_heartbeat`).
pub fn snapshot() -> Vec<(String, String)> {
    active_leases()
        .lock()
        .unwrap()
        .iter()
        .map(|(lease_id, workspace_id)| (lease_id.clone(), workspace_id.clone()))
        .collect()
}

/// Release every lease this registry still tracks. Called from the engine
/// shutdown path (both the signal and RPC branches in `app/server.rs`)
/// right alongside `ServerState::shutdown_workers`, which does the same
/// job for worker panes. Best-effort: a release failure is logged and does
/// not block shutdown — a lease left behind here is still recovered by
/// `crate::ladder_lease_reap` on the next engine startup. Returns the
/// number of leases successfully released.
pub async fn release_all_on_shutdown(cube_client: &dyn CubeClient) -> usize {
    let leases = snapshot();
    if leases.is_empty() {
        return 0;
    }
    tracing::info!(
        count = leases.len(),
        "conflict_ladder: releasing in-flight rung-1 leases on engine shutdown",
    );
    let mut released = 0usize;
    for (lease_id, workspace_id) in leases {
        match cube_client.release_workspace(&lease_id).await {
            Ok(()) => released += 1,
            Err(err) => tracing::warn!(
                lease_id = %lease_id,
                workspace_id = %workspace_id,
                ?err,
                "conflict_ladder: failed to release rung-1 lease on shutdown (best-effort; startup reap will \
                 recover it on the next boot)",
            ),
        }
        unregister(&lease_id);
    }
    released
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(id: &str, workspace_id: &str) -> CubeWorkspaceLease {
        CubeWorkspaceLease {
            lease_id: id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            workspace_path: std::path::PathBuf::from(format!("/tmp/{workspace_id}")),
        }
    }

    /// register → snapshot → unregister round-trips cleanly. Uses a
    /// unique lease id (test name embedded) so this test is safe to run
    /// concurrently with every other test in this process sharing the same
    /// process-wide static.
    #[test]
    fn register_and_unregister_round_trip() {
        let lease = lease("registry-test-lease-1", "ws-registry-test-1");
        register(&lease);
        assert!(
            snapshot()
                .iter()
                .any(|(id, ws)| id == "registry-test-lease-1" && ws == "ws-registry-test-1"),
            "registered lease must appear in the snapshot",
        );
        unregister(&lease.lease_id);
        assert!(
            !snapshot().iter().any(|(id, _)| id == "registry-test-lease-1"),
            "unregistered lease must not appear in the snapshot",
        );
    }

    /// `release_all_on_shutdown` releases every tracked lease via the cube
    /// client and clears the registry. `ACTIVE_LEASES` is a process-wide
    /// static (see the module doc comment on why), so a full-pass call
    /// like this one necessarily drains whatever a concurrently-running
    /// test elsewhere in this binary has registered too — this asserts
    /// only that *our* two leases were released and cleared, not that the
    /// call's effects were scoped to them.
    #[tokio::test]
    async fn release_all_on_shutdown_releases_and_clears_tracked_leases() {
        let lease_a = lease("registry-test-shutdown-a", "ws-shutdown-a");
        let lease_b = lease("registry-test-shutdown-b", "ws-shutdown-b");
        register(&lease_a);
        register(&lease_b);

        let cube = RecordingCube::default();
        release_all_on_shutdown(&cube).await;

        let calls = cube.released.lock().unwrap().clone();
        assert!(calls.contains(&"registry-test-shutdown-a".to_owned()));
        assert!(calls.contains(&"registry-test-shutdown-b".to_owned()));
        assert!(
            !snapshot()
                .iter()
                .any(|(id, _)| id == "registry-test-shutdown-a" || id == "registry-test-shutdown-b")
        );
    }

    /// A release failure is still unregistered (so shutdown never wedges on
    /// a lease cube has already forgotten) and does not count toward the
    /// returned success count. This test's own lease is registered alone
    /// (nothing else is registered concurrently by this test), so
    /// `released == 0` is safe to assert exactly: every candidate the
    /// failing cube double sees this call errors on.
    #[tokio::test]
    async fn release_all_on_shutdown_unregisters_even_on_failure() {
        let lease = lease("registry-test-shutdown-fail", "ws-shutdown-fail");
        register(&lease);

        let cube = RecordingCube {
            fail: true,
            ..RecordingCube::default()
        };
        release_all_on_shutdown(&cube).await;

        assert!(!snapshot().iter().any(|(id, _)| id == "registry-test-shutdown-fail"));
    }

    #[derive(Default)]
    struct RecordingCube {
        released: Mutex<Vec<String>>,
        fail: bool,
    }

    crate::stub_cube_client! { RecordingCube {
        async fn release_workspace(&self, lease_id: &str) -> anyhow::Result<()> {
            if self.fail {
                anyhow::bail!("simulated release failure");
            }
            self.released.lock().unwrap().push(lease_id.to_owned());
            Ok(())
        }
    } }
}
