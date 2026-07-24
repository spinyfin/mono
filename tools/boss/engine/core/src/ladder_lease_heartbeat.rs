//! Belt-and-braces short-TTL heartbeat for cube workspace leases held by an
//! in-flight [`crate::conflict_ladder`] rung-1 attempt.
//!
//! ## Why this exists
//!
//! [`crate::ladder_lease_registry`] (clean shutdown) and
//! [`crate::ladder_lease_reap`] (startup, after a crash) are the primary
//! fixes for the 2026-07-18 incident: a rung-1 lease orphaned by an engine
//! restart sat unreclaimed for cube's full default TTL (1800 s / 30 min)
//! before the engine ever got a chance to notice. Those two fixes bound the
//! *reclaim* latency; this sweep shrinks the *exposure window* itself by
//! refreshing every currently-tracked rung-1 lease down to a much shorter
//! TTL ([`RUNG1_LEASE_TTL_SECS`]) than cube's default — "no human is
//! watching them" is exactly the case
//! [`crate::cube_lease_heartbeat`] exists for on the worker side; rung-1
//! leases have no equivalent coverage at all today, since they carry no
//! `work_executions` row and never touch that module's live-worker-registry
//! sweep.
//!
//! Deliberately a *separate* periodic sweep rather than an inline
//! `heartbeat_lease` call inside `try_mechanical_rungs` itself: rung-1's
//! `CubeClient` test doubles (`ScriptCube`, `Rung0Cube` in
//! `conflict_ladder_tests.rs`) don't implement `heartbeat_lease`, and a
//! rung-1 attempt is normally seconds long anyway (a mechanical rebase, no
//! agent) — there is no real risk of it outliving even a single sweep
//! interval in practice. Keeping the heartbeat out-of-band means this
//! sweep can be added, tuned, or disabled without touching the ladder's
//! hot path or its tests at all.
//!
//! Reuses [`crate::ladder_lease_registry::snapshot`] as its candidate set —
//! the same in-memory tracking `try_mechanical_rungs` already registers
//! into and the shutdown path already drains — so this sweep needs no
//! state of its own.

use std::sync::Arc;
use std::time::Duration;

use crate::coordinator::CubeClient;
use crate::sweep_loop::{SweepOutcome, spawn_sweep_loop};

/// TTL (seconds) this sweep refreshes every tracked rung-1 lease to.
/// Deliberately far below cube's 1800 s default: a rung-1 attempt normally
/// completes in well under a minute (an engine-direct rebase, no agent),
/// so 600 s / 10 min is generous headroom while still shrinking the
/// 2026-07-18 incident's exposure window by 3×.
pub const RUNG1_LEASE_TTL_SECS: u64 = 600;

/// Cadence between passes. Well below [`RUNG1_LEASE_TTL_SECS`] so a lease
/// that is genuinely still in flight gets several refreshes before the
/// tightened TTL could lapse (mirrors the ≪-TTL margin
/// [`crate::cube_lease_heartbeat::DEFAULT_HEARTBEAT_INTERVAL`] keeps
/// against its own TTL).
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(120);

/// Counts from one heartbeat pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LadderHeartbeatOutcome {
    /// Rung-1 leases whose TTL was successfully refreshed this pass.
    pub heartbeated: usize,
    /// Refresh calls that errored (lease already released, cube
    /// unreachable) or the registry was empty. A failure here is
    /// best-effort and non-fatal — the lease still has whatever TTL cube
    /// last gave it, and `crate::ladder_lease_reap` recovers a genuinely
    /// orphaned one at the next engine startup regardless.
    pub failed: usize,
}

impl SweepOutcome for LadderHeartbeatOutcome {
    fn has_activity(&self) -> bool {
        self.heartbeated > 0 || self.failed > 0
    }

    fn log(&self) {
        tracing::info!(
            heartbeated = self.heartbeated,
            failed = self.failed,
            ttl_secs = RUNG1_LEASE_TTL_SECS,
            "ladder-lease heartbeat: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn (via `spawn_sweep_loop`'s contract), which is
/// a harmless no-op whenever the registry is empty — the overwhelmingly
/// common case, since rung-1 attempts are normally over well before the
/// first pass would even fire.
pub fn spawn_loop(cube_client: Arc<dyn CubeClient>, interval: Duration) -> tokio::task::JoinHandle<()> {
    spawn_sweep_loop(interval, move || {
        let cube_client = Arc::clone(&cube_client);
        async move { run_one_pass(cube_client.as_ref()).await }
    })
}

/// Refresh the TTL of every lease [`crate::ladder_lease_registry::snapshot`]
/// currently tracks down to [`RUNG1_LEASE_TTL_SECS`]. Best-effort: an
/// individual failure is logged and does not stop the rest of the pass.
pub async fn run_one_pass(cube_client: &dyn CubeClient) -> LadderHeartbeatOutcome {
    let mut outcome = LadderHeartbeatOutcome::default();
    for (lease_id, workspace_id) in crate::ladder_lease_registry::snapshot() {
        match cube_client.heartbeat_lease(&lease_id, Some(RUNG1_LEASE_TTL_SECS)).await {
            Ok(()) => outcome.heartbeated += 1,
            Err(err) => {
                outcome.failed += 1;
                tracing::warn!(
                    lease_id = %lease_id,
                    workspace_id = %workspace_id,
                    ?err,
                    "ladder-lease heartbeat: failed to refresh rung-1 lease TTL (best-effort)",
                );
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Result;

    use super::*;
    use crate::coordinator::CubeWorkspaceLease;

    #[derive(Default)]
    struct RecordingCube {
        heartbeats: Mutex<Vec<(String, Option<u64>)>>,
        fail: bool,
    }

    crate::stub_cube_client! { RecordingCube {
        async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
            if self.fail {
                anyhow::bail!("simulated heartbeat failure");
            }
            self.heartbeats.lock().unwrap().push((lease_id.to_owned(), ttl_seconds));
            Ok(())
        }
    } }

    /// A tracked lease is heartbeated with the tightened TTL. `heartbeated`
    /// is asserted as "at least our lease", not an exact count: the
    /// registry is a process-wide static (see
    /// `crate::ladder_lease_registry`'s module doc comment), so a
    /// concurrently-running test elsewhere in this binary may have an
    /// entry of its own present in the same pass.
    #[tokio::test]
    async fn heartbeats_tracked_lease_with_short_ttl() {
        let lease = CubeWorkspaceLease {
            lease_id: "heartbeat-test-lease-1".to_owned(),
            workspace_id: "ws-heartbeat-test-1".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/ws-heartbeat-test-1"),
            dirty_verified: None,
        };
        crate::ladder_lease_registry::register(&lease);

        let cube = RecordingCube::default();
        let outcome = run_one_pass(&cube).await;

        crate::ladder_lease_registry::unregister(&lease.lease_id);

        assert!(outcome.heartbeated >= 1, "our registered lease must be heartbeated");
        assert!(
            cube.heartbeats
                .lock()
                .unwrap()
                .iter()
                .any(|(id, ttl)| id == "heartbeat-test-lease-1" && *ttl == Some(RUNG1_LEASE_TTL_SECS)),
            "must heartbeat with the tightened rung-1 TTL, not cube's default",
        );
    }

    /// A heartbeat failure is counted and does not panic.
    #[tokio::test]
    async fn heartbeat_failure_is_best_effort() {
        let lease = CubeWorkspaceLease {
            lease_id: "heartbeat-test-lease-fail".to_owned(),
            workspace_id: "ws-heartbeat-test-fail".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/ws-heartbeat-test-fail"),
            dirty_verified: None,
        };
        crate::ladder_lease_registry::register(&lease);

        let cube = RecordingCube {
            fail: true,
            ..Default::default()
        };
        let outcome = run_one_pass(&cube).await;

        crate::ladder_lease_registry::unregister(&lease.lease_id);

        assert!(outcome.failed >= 1);
    }
}
