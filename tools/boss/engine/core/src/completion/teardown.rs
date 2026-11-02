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
    /// Resolve the cube that owns `execution_id`'s lease through the shared
    /// [`crate::cube_lease_heartbeat::ExecutionCubes`] seam. A remote run with
    /// no resolver installed is an error — it must never fall back to the
    /// local cube.
    pub(super) async fn owning_cube(
        &self,
        execution_id: &str,
    ) -> anyhow::Result<crate::cube_lease_heartbeat::ResolvedExecutionCube> {
        let cubes = self
            .execution_cubes
            .read()
            .expect("execution cubes lock poisoned")
            .clone();
        match cubes {
            Some(cubes) => cubes.cube_for_execution(execution_id).await,
            None => {
                let host = self.work_db.latest_run_host_for_execution(execution_id)?;
                anyhow::ensure!(
                    matches!(host.as_deref(), None | Some("local")),
                    "remote cleanup adapter provider is unavailable"
                );
                Ok(crate::cube_lease_heartbeat::ResolvedExecutionCube {
                    cube: Arc::clone(&self.cube_client),
                    host_id: "local".to_owned(),
                })
            }
        }
    }

    pub(super) async fn recovery_adapter(
        &self,
        execution_id: &str,
    ) -> Option<Arc<dyn boss_execution_contracts::HostAdapter>> {
        let host = self.work_db.execution_host_id(execution_id).ok().flatten()?;
        if host == "local" {
            return None;
        }
        let provider = self
            .host_adapter_provider
            .read()
            .expect("host adapter provider lock poisoned")
            .clone()?;
        crate::host_adapter::resolve_host_adapter(&self.work_db, provider.as_ref(), &host)
            .await
            .ok()
    }

    /// True when `execution_id` is not a remote run, or its remote worker
    /// process is provably gone (`kill -0` on the owning host reports no such
    /// process). A terminal status is NOT that proof: a cancelled remote
    /// worker keeps running, and `HostAdapter` has no stop capability. A
    /// missing pid, an unreachable host, or an unavailable adapter is
    /// inconclusive and returns `false`, preserving the lease and live state
    /// until [`crate::remote_lease_reconcile`] sees positive death. That
    /// reconciler separately bounds terminal persona retention.
    pub(crate) async fn remote_worker_proven_gone(&self, execution_id: &str) -> bool {
        let handle = match self.work_db.latest_remote_run_for_execution(execution_id) {
            Ok(None) => return true,
            Ok(Some(handle)) => handle,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "remote worker liveness: run lookup failed; assuming alive"
                );
                return false;
            }
        };
        let Some(remote_pid) = handle.remote_pid else {
            return false;
        };
        let provider = self
            .host_adapter_provider
            .read()
            .expect("host adapter provider lock poisoned")
            .clone();
        let Some(provider) = provider else {
            return false;
        };
        let Ok(adapter) =
            crate::host_adapter::resolve_host_adapter(&self.work_db, provider.as_ref(), &handle.host_id).await
        else {
            return false;
        };
        if !matches!(adapter.probe_remote_worker_alive(remote_pid).await, Ok(Some(false))) {
            return false;
        }
        if let Err(error) =
            crate::remote_input_lifecycle::cleanup_execution_inputs(&self.work_db, adapter.as_ref(), execution_id).await
        {
            tracing::warn!(?error, "remote input cleanup failed; retaining lease for retry");
            return false;
        }
        true
    }

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
    /// Local `teardown_driver_workspace` is still called when it is `None`,
    /// since a driver may key its state off the recorded runtime state alone.
    /// Remote runs skip local driver teardown and release through their host's
    /// adapter; their workspace paths are never accessed on this host.
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
        work_item_id: &str,
        lease_id: Option<&str>,
        workspace_path: Option<&Path>,
        path: &'static str,
        guard: TeardownGuard,
    ) {
        // Every terminal completion path clears its worker-declared wait here.
        self.wait_registry.forget(execution_id);
        let started = Instant::now();

        let pane_started = Instant::now();
        // A Stop can precede remote process exit. Keep the virtual slot and
        // persona until the shared release guard proves death; remote lease
        // reconciliation retries this release after the process exits.
        let pane_outcome = self.pane_releaser.release_pane(execution_id).await;
        let pane_ms = pane_started.elapsed().as_millis();

        let driver_started = Instant::now();
        let owning_cube = self.owning_cube(execution_id).await;
        if matches!(&owning_cube, Ok(resolved) if resolved.is_local()) {
            crate::driver_teardown::teardown_driver_workspace(
                &self.work_db,
                execution_id,
                workspace_path,
                crate::driver_teardown::TeardownReason::Completion(path),
            )
            .await;
        }
        let driver_ms = driver_started.elapsed().as_millis();

        let cube_started = Instant::now();
        let mut cube_timed_out = false;
        let mut lease_released = false;
        if let Some(lease_id) = lease_id {
            let remote = match &owning_cube {
                Ok(resolved) if !resolved.is_local() => Some(Arc::clone(&resolved.cube)),
                _ => None,
            };
            let remote_host = if remote.is_some() {
                self.recovery_adapter(execution_id).await
            } else {
                None
            };
            let pane_status = match pane_outcome {
                PaneReleaseOutcome::Reaped => crate::workspace_lease_release::PaneStatus::Gone,
                PaneReleaseOutcome::NoLiveWorker => crate::workspace_lease_release::PaneStatus::Present,
            };
            let release = async {
                match owning_cube {
                    Ok(resolved) if resolved.is_local() => {
                        self.cube_client.release_workspace_with_reset(lease_id).await
                    }
                    Ok(resolved) => resolved.cube.force_release_lease(lease_id, Some(path)).await,
                    Err(err) => Err(err),
                }
            };
            let timed = async {
                match tokio::time::timeout(CUBE_RELEASE_TIMEOUT, release).await {
                    Ok(result) => result,
                    Err(_elapsed) => {
                        cube_timed_out = true;
                        Err(anyhow::anyhow!(
                            "cube workspace release timed out after {}s",
                            CUBE_RELEASE_TIMEOUT.as_secs()
                        ))
                    }
                }
            };
            match self.work_db.get_execution(execution_id) {
                Ok(execution) => {
                    let outcome = if let Some(cube) = remote {
                        // A remote worker cannot be stopped from here, so a
                        // finished turn is not proof its workspace is idle:
                        // gate on the owning host's occupancy and recovery.
                        match remote_host {
                            Some(host) => {
                                crate::remote_lease_reconcile::release_remote_lease(
                                    &self.work_db,
                                    host.as_ref(),
                                    execution_id,
                                    pane_status,
                                    |lease_id| async move {
                                        tokio::time::timeout(
                                            CUBE_RELEASE_TIMEOUT,
                                            cube.force_release_lease(&lease_id, Some(path)),
                                        )
                                        .await
                                        .unwrap_or_else(|_| {
                                            Err(anyhow::anyhow!(
                                                "cube workspace release timed out after {}s",
                                                CUBE_RELEASE_TIMEOUT.as_secs()
                                            ))
                                        })
                                    },
                                )
                                .await
                            }
                            None => crate::workspace_lease_release::LeaseReleaseOutcome::Kept {
                                reason: crate::workspace_lease_release::KeepReason::OccupancyUnconfirmed,
                            },
                        }
                    } else {
                        crate::workspace_lease_release::apply_release(
                            &self.work_db,
                            &execution,
                            lease_id,
                            crate::workspace_lease_release::ReleaseEvidence {
                                cause: crate::workspace_lease_release::TerminalCause::ProvenCompleted,
                                pane: pane_status,
                                occupancy: &crate::workspace_lease_release::EmptyOccupancy,
                                bookmark: &crate::workspace_lease_release::DbBookmarkGate {
                                    db: &self.work_db,
                                    adapter: None,
                                },
                            },
                            timed,
                        )
                        .await
                    };
                    lease_released = outcome == crate::workspace_lease_release::LeaseReleaseOutcome::Released;
                    if !matches!(
                        outcome,
                        crate::workspace_lease_release::LeaseReleaseOutcome::Released
                            | crate::workspace_lease_release::LeaseReleaseOutcome::NoLease
                    ) {
                        tracing::error!(
                            execution_id,
                            lease_id,
                            outcome = ?outcome,
                            "worker teardown: cube lease was not released",
                        );
                    }
                }
                Err(err) => tracing::error!(
                    execution_id,
                    path,
                    lease_id,
                    ?err,
                    "worker teardown: execution lookup failed; cube release skipped",
                ),
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

        // The dispatch timeline's explicit end-of-run marker (see
        // `Stage::ExecutionFinalized`). Emitted here, from the one choke
        // point every completion route runs, so the per-execution timeline
        // records that the run ended on the PR-producing path and the
        // declared-no-op path alike — the dispatch stage tracker observes
        // the true terminal state rather than inferring it from silence.
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::ExecutionFinalized, DispatchOutcome::Ok, execution_id)
                    .with_work_item(work_item_id)
                    .with_details(serde_json::json!({
                        "path": path,
                        "pane_outcome": format!("{pane_outcome:?}"),
                        "released_lease": lease_released,
                        "cube_timed_out": cube_timed_out,
                        "release_pane_ms": pane_ms,
                        "driver_teardown_ms": driver_ms,
                        "cube_release_ms": cube_ms,
                        "total_ms": started.elapsed().as_millis(),
                    })),
            )
            .await;

        // Explicit rather than implicit: the sweep may reclaim this slot
        // again only now that the pane is genuinely gone.
        drop(guard);
    }
}
