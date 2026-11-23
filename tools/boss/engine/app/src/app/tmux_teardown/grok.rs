//! Pre-reap ACP turn settlement and token capture for Grok tmux workers.
//!
//! A Grok worker runs its task inside one ACP prompt turn. When a completion
//! proposal (`boss propose done`) or another engine-initiated teardown lands
//! while that turn is still open, killing the tmux session immediately
//! prevents Grok from emitting its ACP `turn_completed` record (and usage
//! totals) to `updates.jsonl`.
//!
//! Before destroying a verified-live Grok session, this module checks
//! `events.jsonl` and `updates.jsonl`, sends the driver's interrupt gesture
//! (ACP `session/cancel`) if a turn is still open, waits up to a short bounded
//! window for `turn_completed` to land and be ingested by [`RunCostCapture`],
//! and records a specific `usage_unavailable` reason if the bound expires
//! without `turn_completed`. The deadline covers the whole pre-reap settle,
//! including driver lookup, path resolution, the initial reads, the interrupt
//! send, polling, and the final ingest. Synchronous WorkDb work runs off the
//! runtime thread; pending persistence is detached so it cannot delay kill.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use boss_engine_driver::AgentDriver;
use boss_engine_driver::grok::{
    GROK_TEARDOWN_POLL_INTERVAL, GROK_TEARDOWN_SETTLE_WINDOW, GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON, GrokSettleStatus,
    GrokSettleTails, GrokSettleTransport, GrokTurnProgress, settle_open_turn, timeout_remaining,
};
use boss_engine_storage_values::RunCostSnapshot;
use boss_tmux::Tmux;

use super::*;

/// Specific `usage_unavailable` reason when `turn_completed` is present but
/// [`RunCostCapture`] could not persist it.
pub(crate) const GROK_TEARDOWN_INGEST_FAILED_REASON: &str = "turn_completed observed but usage capture failed";

#[derive(Clone)]
struct GrokTelemetryPaths {
    events_path: Option<PathBuf>,
    updates_path: Option<PathBuf>,
    containment_root: Option<Option<PathBuf>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrokIngestOutcome {
    Persisted,
    NoUsageRecord,
    Failed,
}

struct GrokIngestWait {
    expect_usage: bool,
    turn_completed: usize,
    already_complete: bool,
}

struct ResolvedGrokTeardown {
    driver: Arc<dyn AgentDriver>,
    paths: GrokTelemetryPaths,
}

impl ServerState {
    #[cfg(test)]
    pub(crate) fn set_grok_teardown_settle_window_for_test(&self, window: Duration) {
        *self.grok_teardown_settle_window_override.lock().unwrap() = Some(window);
    }

    fn grok_teardown_settle_window(&self) -> Duration {
        #[cfg(test)]
        if let Some(window) = *self.grok_teardown_settle_window_override.lock().unwrap() {
            return window;
        }
        GROK_TEARDOWN_SETTLE_WINDOW
    }

    #[cfg(test)]
    pub(crate) fn set_grok_ingest_gate_for_test(
        &self,
        started: std::sync::mpsc::SyncSender<()>,
        proceed: std::sync::mpsc::Receiver<()>,
    ) {
        *self.grok_ingest_gate.lock().unwrap() = Some((started, proceed));
    }

    #[cfg(test)]
    fn take_grok_ingest_gate_for_test(
        &self,
    ) -> Option<(std::sync::mpsc::SyncSender<()>, std::sync::mpsc::Receiver<()>)> {
        self.grok_ingest_gate.lock().unwrap().take()
    }

    /// Best-effort one-shot cost capture for a Grok session whose process is
    /// already dead or missing when teardown runs, so any `turn_completed`
    /// flushed just before exit is still ingested. Bounded by the settle
    /// window; expiry is logged and does not delay kill/slot release.
    pub(super) async fn capture_grok_cost_if_present(
        &self,
        execution_id: &str,
        session_name: &str,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let settle_window = self.grok_teardown_settle_window();
        let deadline = tokio::time::Instant::now() + settle_window;
        let resolved = self.resolve_grok_teardown_bounded(execution_id, deadline).await?;
        self.finish_dead_or_missing_capture(execution_id, session_name, &resolved, deadline)
            .await
    }

    /// Ensure an open Grok ACP turn is given a chance to close (via the
    /// driver's interrupt plan / `session/cancel`) and emit `turn_completed`
    /// before `kill_session_verified` destroys the session.
    ///
    /// Driver lookup, path resolution and snapshot writes run off the runtime
    /// thread under `deadline`. Returns an optional `JoinHandle` for a
    /// background DB write of `usage_unavailable` so `kill_session_verified`
    /// is never delayed by SQLite contention.
    pub(super) async fn settle_grok_turn_before_reap(
        &self,
        tmux: &Tmux,
        execution_id: &str,
        session_name: &str,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let settle_window = self.grok_teardown_settle_window();
        let deadline = tokio::time::Instant::now() + settle_window;
        let resolved = self.resolve_grok_teardown_bounded(execution_id, deadline).await?;
        self.settle_resolved_grok_turn(tmux, execution_id, session_name, resolved, deadline, settle_window)
            .await
    }

    async fn resolve_grok_teardown_bounded(
        &self,
        execution_id: &str,
        deadline: tokio::time::Instant,
    ) -> Option<ResolvedGrokTeardown> {
        let work_db = self.work_db.clone();
        let cache = self.transcript_path_cache.clone();
        let exec_id = execution_id.to_owned();
        match timeout_remaining(
            deadline,
            tokio::task::spawn_blocking(move || resolve_grok_teardown(&work_db, &cache, &exec_id)),
        )
        .await
        {
            Some(Ok(resolved)) => resolved,
            Some(Err(_)) | None => {
                tracing::warn!(
                    execution_id,
                    "reap_tmux_worker: resolving teardown context timed out; proceeding with reap",
                );
                None
            }
        }
    }

    async fn settle_resolved_grok_turn(
        &self,
        tmux: &Tmux,
        execution_id: &str,
        session_name: &str,
        resolved: ResolvedGrokTeardown,
        deadline: tokio::time::Instant,
        settle_window: Duration,
    ) -> Option<tokio::task::JoinHandle<()>> {
        struct LocalSettleTransport<'a> {
            tails: GrokSettleTails,
            tmux: &'a Tmux,
            session_name: &'a str,
            execution_id: &'a str,
        }

        #[async_trait::async_trait]
        impl GrokSettleTransport for LocalSettleTransport<'_> {
            type Error = anyhow::Error;

            async fn poll(&mut self, progress: &mut GrokTurnProgress) {
                self.tails.poll(progress).await;
            }

            async fn send_key(&mut self, key: &'static str) -> Result<(), Self::Error> {
                self.tmux.send_key(self.session_name, key).await
            }

            fn on_cancel_start(&mut self, progress: &GrokTurnProgress) {
                tracing::info!(
                    execution_id = self.execution_id,
                    session = self.session_name,
                    turn_started = progress.events.started,
                    turn_ended = progress.events.ended,
                    turn_completed = progress.updates.completed,
                    "reap_tmux_worker: cancelling open Grok ACP turn before reap",
                );
            }

            fn on_send_error(&mut self, err: Self::Error) {
                tracing::warn!(
                    execution_id = self.execution_id,
                    session = self.session_name,
                    error = %format!("{err:#}"),
                    "reap_tmux_worker: failed sending interrupt to Grok session before reap; skipping settle wait",
                );
            }
        }

        let mut transport = LocalSettleTransport {
            tails: GrokSettleTails::new(
                resolved.paths.events_path.clone(),
                resolved.paths.updates_path.clone(),
                "reap_tmux_worker",
            ),
            tmux,
            session_name,
            execution_id,
        };
        let result = settle_open_turn(
            deadline,
            GROK_TEARDOWN_POLL_INTERVAL,
            resolved.driver.interrupt_plan(),
            &mut transport,
        )
        .await;

        match result.status {
            GrokSettleStatus::Skipped => None,
            GrokSettleStatus::Completed {
                completed,
                already_complete,
            } => {
                self.finish_ingest(
                    execution_id,
                    session_name,
                    resolved.paths.clone(),
                    deadline,
                    GrokIngestWait {
                        expect_usage: true,
                        turn_completed: completed,
                        already_complete,
                    },
                )
                .await
            }
            GrokSettleStatus::TimedOut => {
                Some(self.record_grok_teardown_unavailable(execution_id, session_name, &result.progress, settle_window))
            }
        }
    }

    async fn finish_dead_or_missing_capture(
        &self,
        execution_id: &str,
        session_name: &str,
        resolved: &ResolvedGrokTeardown,
        deadline: tokio::time::Instant,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let mut progress = GrokTurnProgress::default();
        let mut tails = GrokSettleTails::new(
            resolved.paths.events_path.clone(),
            resolved.paths.updates_path.clone(),
            "reap_tmux_worker",
        );
        let _ = timeout_remaining(deadline, tails.poll(&mut progress)).await;
        self.finish_ingest(
            execution_id,
            session_name,
            resolved.paths.clone(),
            deadline,
            GrokIngestWait {
                expect_usage: progress.updates.completed > 0,
                turn_completed: progress.updates.completed,
                already_complete: false,
            },
        )
        .await
    }

    /// Spawn transcript-path write plus capture as one detached task, then
    /// wait only for its handle under the remaining settle budget. Dropping
    /// the wait on timeout leaves the task running so usage already on disk
    /// is not lost; a later token write clears `usage_unavailable`.
    async fn finish_ingest(
        &self,
        execution_id: &str,
        session_name: &str,
        paths: GrokTelemetryPaths,
        deadline: tokio::time::Instant,
        wait: GrokIngestWait,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let failure_reason = wait.expect_usage.then_some(GROK_TEARDOWN_INGEST_FAILED_REASON);
        let ingest = self.spawn_ingest_grok_updates(execution_id, paths, failure_reason);
        match timeout_remaining(deadline, ingest).await {
            Some(Ok(GrokIngestOutcome::Persisted)) => {
                if wait.expect_usage {
                    tracing::info!(
                        execution_id,
                        session = session_name,
                        turn_completed = wait.turn_completed,
                        already_complete = wait.already_complete,
                        "reap_tmux_worker: captured Grok turn_completed before reap",
                    );
                }
                None
            }
            Some(Ok(_)) if !wait.expect_usage => None,
            Some(Ok(_)) => {
                tracing::warn!(
                    execution_id,
                    session = session_name,
                    turn_completed = wait.turn_completed,
                    reason = GROK_TEARDOWN_INGEST_FAILED_REASON,
                    "reap_tmux_worker: Grok turn_completed was present but usage capture failed",
                );
                None
            }
            Some(Err(_)) => {
                tracing::warn!(
                    execution_id,
                    session = session_name,
                    "reap_tmux_worker: Grok usage capture task failed",
                );
                wait.expect_usage.then(|| {
                    self.spawn_persist_grok_usage_unavailable(execution_id, GROK_TEARDOWN_INGEST_FAILED_REASON)
                })
            }
            None => {
                tracing::warn!(
                    execution_id,
                    session = session_name,
                    turn_completed = wait.turn_completed,
                    "reap_tmux_worker: Grok usage capture still running after teardown bound; proceeding with reap",
                );
                None
            }
        }
    }

    fn record_grok_teardown_unavailable(
        &self,
        execution_id: &str,
        session_name: &str,
        progress: &GrokTurnProgress,
        settle_window: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tracing::warn!(
            execution_id,
            session = session_name,
            turn_started = progress.events.started,
            turn_ended = progress.events.ended,
            turn_completed = progress.updates.completed,
            settle_window_secs = settle_window.as_secs_f64(),
            reason = GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON,
            "reap_tmux_worker: Grok turn_completed did not arrive before teardown bound; proceeding with reap",
        );
        self.spawn_persist_grok_usage_unavailable(execution_id, GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON)
    }

    fn spawn_persist_grok_usage_unavailable(&self, execution_id: &str, reason: &str) -> tokio::task::JoinHandle<()> {
        let work_db = self.work_db.clone();
        let execution_id = execution_id.to_owned();
        let reason = reason.to_owned();
        tokio::task::spawn_blocking(move || persist_grok_usage_unavailable(&work_db, &execution_id, reason))
    }

    fn spawn_ingest_grok_updates(
        &self,
        execution_id: &str,
        paths: GrokTelemetryPaths,
        failure_reason: Option<&'static str>,
    ) -> tokio::task::JoinHandle<GrokIngestOutcome> {
        let capture = Arc::clone(&self.run_cost_capture);
        let work_db = self.work_db.clone();
        let execution_id = execution_id.to_owned();
        #[cfg(test)]
        let ingest_gate = self.take_grok_ingest_gate_for_test();
        tokio::task::spawn(async move {
            #[cfg(test)]
            if let Some((started, proceed)) = ingest_gate {
                let _ = started.send(());
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = proceed.recv();
                })
                .await;
            }
            let outcome = ingest_grok_updates(&capture, work_db.clone(), &execution_id, &paths).await;
            if !matches!(outcome, GrokIngestOutcome::Persisted)
                && let Some(reason) = failure_reason
            {
                let persist_db = work_db.clone();
                let persist_id = execution_id.clone();
                let persist_reason = reason.to_owned();
                let _ = tokio::task::spawn_blocking(move || {
                    persist_grok_usage_unavailable(&persist_db, &persist_id, persist_reason);
                })
                .await;
            }
            outcome
        })
    }
}

fn persist_grok_usage_unavailable(work_db: &boss_work_db::WorkDb, execution_id: &str, reason: String) {
    let snapshot = RunCostSnapshot {
        usage_unavailable: Some(reason),
        ..Default::default()
    };
    if let Err(err) = work_db.set_run_cost_snapshot(execution_id, snapshot) {
        tracing::warn!(
            execution_id,
            error = %format!("{err:#}"),
            "reap_tmux_worker: failed recording Grok teardown usage_unavailable reason",
        );
    }
}

async fn ingest_grok_updates(
    capture: &boss_engine_runtime::run_cost::RunCostCapture,
    work_db: Arc<boss_work_db::WorkDb>,
    execution_id: &str,
    paths: &GrokTelemetryPaths,
) -> GrokIngestOutcome {
    let Some(updates_path) = paths.updates_path.clone() else {
        return GrokIngestOutcome::NoUsageRecord;
    };
    let Some(containment_root) = paths.containment_root.clone() else {
        return GrokIngestOutcome::NoUsageRecord;
    };
    match tokio::fs::try_exists(&updates_path).await {
        Ok(false) => return GrokIngestOutcome::NoUsageRecord,
        Err(error) => {
            tracing::warn!(
                execution_id,
                transcript_path = %updates_path.display(),
                %error,
                "reap_tmux_worker: Grok updates.jsonl exists check failed",
            );
            return GrokIngestOutcome::Failed;
        }
        Ok(true) => {}
    }
    let path_str = updates_path.to_string_lossy().into_owned();
    let db = Arc::clone(&work_db);
    let exec_id = execution_id.to_owned();
    let _ = tokio::task::spawn_blocking(move || db.set_run_transcript_path_if_unset(&exec_id, &path_str)).await;
    match capture
        .capture_and_persist(&work_db, execution_id, &updates_path, containment_root.as_deref())
        .await
    {
        Ok(true) => GrokIngestOutcome::Persisted,
        Ok(false) => GrokIngestOutcome::NoUsageRecord,
        Err(error) => {
            tracing::warn!(
                execution_id,
                %error,
                "reap_tmux_worker: failed capturing Grok run cost from updates.jsonl",
            );
            GrokIngestOutcome::Failed
        }
    }
}

fn resolve_grok_teardown(
    work_db: &boss_work_db::WorkDb,
    transcript_path_cache: &boss_engine_runtime::live_status_loop::TranscriptPathCache,
    execution_id: &str,
) -> Option<ResolvedGrokTeardown> {
    let driver = boss_engine_completion::driver_transcript::driver_for_spawned_execution(work_db, execution_id)?;
    if driver.descriptor().name != boss_engine_driver::grok::TRANSCRIPT_DRIVER_SLUG {
        return None;
    }
    let paths = resolve_grok_telemetry_paths(work_db, transcript_path_cache, execution_id, driver.as_ref());
    Some(ResolvedGrokTeardown { driver, paths })
}

fn resolve_grok_telemetry_paths(
    work_db: &boss_work_db::WorkDb,
    transcript_path_cache: &boss_engine_runtime::live_status_loop::TranscriptPathCache,
    execution_id: &str,
    driver: &dyn AgentDriver,
) -> GrokTelemetryPaths {
    let session_files = boss_engine_driver::grok::session_files_for_run(execution_id);
    let updates_path = transcript_path_cache
        .get(execution_id)
        .map(PathBuf::from)
        .or_else(|| {
            work_db
                .transcript_path_for_execution(execution_id)
                .ok()
                .flatten()
                .map(PathBuf::from)
        })
        .or_else(|| session_files.as_ref().map(|(_, updates)| updates.clone()));
    let events_path = session_files.map(|(events, _)| events).or_else(|| {
        updates_path
            .as_ref()
            .map(|updates| updates.with_file_name("events.jsonl"))
    });
    let containment_root = match driver.transcript_containment_root(execution_id) {
        Ok(root) => Some(root),
        Err(error) => {
            tracing::warn!(
                execution_id,
                %error,
                "reap_tmux_worker: refusing Grok cost capture because transcript containment could not be resolved",
            );
            None
        }
    };
    GrokTelemetryPaths {
        events_path,
        updates_path,
        containment_root,
    }
}
