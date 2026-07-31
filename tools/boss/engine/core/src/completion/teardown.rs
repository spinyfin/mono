//! The one worker-teardown sequence every completion path runs after it
//! terminalizes an execution row.
//!
//! ## Why this is shared rather than open-coded per path
//!
//! Six paths used to hand-roll this tail —
//! [`super::WorkerCompletionHandler::finalize_pr_transition`],
//! `finalize_pr_review_pass`, `finalize_automation_triage`,
//! `finalize_answer_agent`, the no-op completion, the driver-terminal-error
//! completion, and the idle-park finalize — and they had drifted into three
//! different orderings of the same three side effects. That drift is what
//! produced the 2026-07-30 incident: five of them ran the unbounded
//! `cube workspace release` subprocess BEFORE the pane release, so the
//! execution sat terminal-with-a-live-pane for however long cube took (388 s
//! in the retained trace) and [`crate::terminal_work_sweep`] reaped and
//! SIGKILLed live workers mid-completion.
//!
//! ## The ordering, and why it is this ordering
//!
//! 1. **`release_pane`** — first, because the pane is the resource every
//!    sweep reclaims. Putting it ahead of the cube call means no unbounded
//!    await can ever sit between "execution is terminal" and "pane is gone".
//!    This is the order [`super::WorkerCompletionHandler::force_release`]
//!    has always used; these paths are the drift, not it.
//! 2. **`teardown_driver_workspace`** — strictly AFTER the pane release.
//!    The worker process is alive until `release_pane` reaps it, and a
//!    still-live Codex process would otherwise refresh its auth token again
//!    after the per-run credential file had already been adopted back to the
//!    source, silently dropping that refresh.
//! 3. **cube workspace release** — last, and timeout-bounded. Last because
//!    driver teardown may still need to read out of the workspace, and cube
//!    is free to hand a released workspace to another lease immediately.
//!
//! The whole sequence runs while the caller holds a
//! [`crate::teardown_registry::TeardownGuard`], so the sweep can tell this
//! pane's own teardown apart from a strand for as long as it takes.

use std::path::Path;
use std::time::{Duration, Instant};

use super::*;
use crate::teardown_registry::TeardownGuard;

/// Upper bound on one `cube workspace release` subprocess.
///
/// Cube being slow is an input the engine must tolerate, not a thing to
/// wait on indefinitely: the retained 2026-07-30 window shows cube's own
/// push-gate taking 138 s and lease attempts failing for minutes at a
/// stretch, and the completion tail behind this call also owns doc
/// detection, event publication and followup reconciliation. Two minutes is
/// comfortably above a healthy release and still bounded. On expiry the
/// future is dropped, which — because the cube subprocess is spawned with
/// `kill_on_drop(true)` — also kills the stray process rather than leaving
/// it parented to the engine.
pub(super) const CUBE_RELEASE_TIMEOUT: Duration = Duration::from_secs(120);

impl WorkerCompletionHandler {
    /// Mark `execution_id` as tearing down. Call this **before** the write
    /// that terminalizes the execution: the mark and the terminal status
    /// must never be observable out of order, or a sweep can still catch
    /// the row bare.
    pub(super) fn begin_teardown(&self, execution_id: &str) -> TeardownGuard {
        self.teardown_registry.begin(execution_id)
    }

    /// Release the worker's pane, tear down driver-owned out-of-workspace
    /// state, and free the cube workspace lease — in that order, with
    /// per-stage timings — then drop `guard` so sweeps may reclaim the
    /// slot again.
    ///
    /// `lease_id` is `None` when the terminalizing write found no lease to
    /// release (already cleared by a racing teardown). `workspace_path` is
    /// the path captured BEFORE the terminalizing write nulled it;
    /// `teardown_driver_workspace` is still called when it is `None`,
    /// since a driver may key its state off the recorded runtime state
    /// alone.
    ///
    /// `path` names the completion path for the log lines (`"stop"`,
    /// `"pr_recheck"`, `"no_op"`, …) so a stall is attributable to a
    /// specific stage of a specific path instead of being inferred from
    /// the gap between two unrelated log lines — which is exactly how the
    /// 388 s stall had to be measured. It is `&'static str` because it is
    /// also forwarded verbatim as
    /// [`crate::driver_teardown::TeardownReason::Completion`]'s payload; every
    /// caller already passes a literal (or a `&'static str` `source`).
    pub(super) async fn finish_worker_teardown(
        &self,
        execution_id: &str,
        lease_id: Option<&str>,
        workspace_path: Option<&Path>,
        path: &'static str,
        guard: TeardownGuard,
    ) {
        let started = Instant::now();

        let pane_started = Instant::now();
        let pane_outcome = self.pane_releaser.release_pane(execution_id).await;
        let pane_ms = pane_started.elapsed().as_millis();

        let driver_started = Instant::now();
        crate::driver_teardown::teardown_driver_workspace(
            &self.work_db,
            execution_id,
            workspace_path,
            crate::driver_teardown::TeardownReason::Completion(path),
        )
        .await;
        let driver_ms = driver_started.elapsed().as_millis();

        let cube_started = Instant::now();
        let mut cube_timed_out = false;
        if let Some(lease_id) = lease_id {
            match tokio::time::timeout(CUBE_RELEASE_TIMEOUT, self.cube_client.release_workspace(lease_id)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::error!(
                    execution_id,
                    path,
                    lease_id,
                    ?err,
                    elapsed_ms = cube_started.elapsed().as_millis(),
                    "worker teardown: cube workspace release failed",
                ),
                Err(_elapsed) => {
                    cube_timed_out = true;
                    // The lease columns were already cleared by the
                    // terminalizing write, so nothing downstream will retry
                    // this release. Cube's own lease TTL reclaims it once the
                    // engine's heartbeat stops (it stops the moment the
                    // execution goes terminal), so the workspace is not lost
                    // — but the operator should see that it took the slow
                    // path.
                    tracing::error!(
                        execution_id,
                        path,
                        lease_id,
                        timeout_secs = CUBE_RELEASE_TIMEOUT.as_secs(),
                        "worker teardown: cube workspace release timed out and was abandoned; \
                         the lease is left to cube's TTL reclaim",
                    );
                }
            }
        }
        let cube_ms = cube_started.elapsed().as_millis();

        tracing::info!(
            execution_id,
            path,
            pane_outcome = ?pane_outcome,
            lease_id,
            cube_timed_out,
            release_pane_ms = pane_ms,
            driver_teardown_ms = driver_ms,
            cube_release_ms = cube_ms,
            total_ms = started.elapsed().as_millis(),
            "worker teardown complete",
        );

        // Explicit rather than implicit: the sweep may reclaim this slot
        // again only now that the pane is genuinely gone.
        drop(guard);
    }
}
