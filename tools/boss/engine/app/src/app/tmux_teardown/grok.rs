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
//! including path resolution, the initial reads, the interrupt send, polling,
//! and the final ingest.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use boss_engine_driver::AgentDriver;
use boss_engine_driver::grok::{
    GROK_TEARDOWN_POLL_INTERVAL, GROK_TEARDOWN_SETTLE_WINDOW, GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON, GrokSettleStatus,
    GrokSettleTransport, GrokTurnProgress, settle_open_turn, timeout_remaining,
};
use boss_engine_runtime::transcript_tail::TranscriptTail;
use boss_engine_storage_values::RunCostSnapshot;
use boss_tmux::Tmux;
use serde_json::Value;

use super::*;

/// Specific `usage_unavailable` reason when `turn_completed` is present but
/// [`RunCostCapture`] could not persist it.
pub(crate) const GROK_TEARDOWN_INGEST_FAILED_REASON: &str = "turn_completed observed but usage capture failed";

struct GrokTelemetryPaths {
    events_path: Option<PathBuf>,
    updates_path: Option<PathBuf>,
    containment_root: Option<Option<PathBuf>>,
}

struct GrokJsonlTail {
    tail: Option<TranscriptTail>,
    generation: u64,
}

struct GrokSettleTails {
    events: GrokJsonlTail,
    updates: GrokJsonlTail,
}

impl GrokSettleTails {
    fn new(paths: &GrokTelemetryPaths) -> Self {
        Self {
            events: GrokJsonlTail {
                tail: paths.events_path.as_ref().map(|path| TranscriptTail::new(path.clone())),
                generation: 0,
            },
            updates: GrokJsonlTail {
                tail: paths
                    .updates_path
                    .as_ref()
                    .map(|path| TranscriptTail::new(path.clone())),
                generation: 0,
            },
        }
    }

    async fn poll(&mut self, progress: &mut GrokTurnProgress) {
        if let Some(tail) = self.events.tail.as_mut() {
            let values = poll_tail(tail).await;
            if tail.generation() != self.events.generation {
                progress.reset_events();
                self.events.generation = tail.generation();
            }
            progress.fold_events(values);
        }
        if let Some(tail) = self.updates.tail.as_mut() {
            let values = poll_tail(tail).await;
            if tail.generation() != self.updates.generation {
                progress.reset_updates();
                self.updates.generation = tail.generation();
            }
            progress.fold_updates(values);
        }
    }
}

impl ServerState {
    /// Returns whether `execution_id` was launched (or resolves) as a Grok
    /// worker, using the same spawn-stamp helper every other post-spawn
    /// driver lookup uses.
    pub(super) fn is_grok_execution(&self, execution_id: &str) -> bool {
        self.grok_driver_for_execution(execution_id).is_some()
    }

    fn grok_driver_for_execution(&self, execution_id: &str) -> Option<Arc<dyn AgentDriver>> {
        let driver =
            boss_engine_completion::driver_transcript::driver_for_spawned_execution(&self.work_db, execution_id)?;
        (driver.descriptor().name == boss_engine_driver::grok::TRANSCRIPT_DRIVER_SLUG).then_some(driver)
    }

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

    /// Best-effort one-shot cost capture for a Grok session whose process is
    /// already dead/missing when teardown runs, so any `turn_completed` flushed
    /// just before exit is still ingested.
    pub(super) async fn capture_grok_cost_if_present(&self, execution_id: &str) {
        let Some(driver) = self.grok_driver_for_execution(execution_id) else {
            return;
        };
        let work_db = self.work_db.clone();
        let cache = self.transcript_path_cache.clone();
        let exec_id = execution_id.to_owned();
        let Ok(paths) = tokio::task::spawn_blocking(move || {
            resolve_grok_telemetry_paths(&work_db, &cache, &exec_id, driver.as_ref())
        })
        .await
        else {
            return;
        };
        let _ = self.ingest_grok_updates(execution_id, &paths).await;
    }

    /// Ensure an open Grok ACP turn is given a chance to close (via the
    /// driver's interrupt plan / `session/cancel`) and emit `turn_completed`
    /// before `kill_session_verified` destroys the session.
    ///
    /// Returns an optional `JoinHandle` for a background DB write of
    /// `usage_unavailable` so `kill_session_verified` is never delayed by SQLite
    /// contention.
    pub(super) async fn settle_grok_turn_before_reap(
        &self,
        tmux: &Tmux,
        execution_id: &str,
        session_name: &str,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let settle_window = self.grok_teardown_settle_window();
        let deadline = tokio::time::Instant::now() + settle_window;
        let driver = self.grok_driver_for_execution(execution_id)?;
        let work_db = self.work_db.clone();
        let cache = self.transcript_path_cache.clone();
        let exec_id = execution_id.to_owned();
        let driver_for_paths = Arc::clone(&driver);
        let paths = match timeout_remaining(
            deadline,
            tokio::task::spawn_blocking(move || {
                resolve_grok_telemetry_paths(&work_db, &cache, &exec_id, driver_for_paths.as_ref())
            }),
        )
        .await
        {
            Some(Ok(paths)) => paths,
            Some(Err(_)) | None => {
                tracing::warn!(
                    execution_id,
                    session = session_name,
                    settle_window_secs = settle_window.as_secs_f64(),
                    reason = GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON,
                    "reap_tmux_worker: resolving Grok telemetry paths timed out; proceeding with reap",
                );
                return Some(
                    self.spawn_persist_grok_usage_unavailable(execution_id, GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON),
                );
            }
        };

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
            tails: GrokSettleTails::new(&paths),
            tmux,
            session_name,
            execution_id,
        };
        let result = settle_open_turn(
            deadline,
            GROK_TEARDOWN_POLL_INTERVAL,
            driver.interrupt_plan(),
            &mut transport,
        )
        .await;

        match result.status {
            GrokSettleStatus::Skipped => None,
            GrokSettleStatus::Completed {
                completed,
                already_complete,
            } => {
                self.finish_grok_settle_ingest(
                    execution_id,
                    session_name,
                    &paths,
                    completed,
                    deadline,
                    already_complete,
                )
                .await
            }
            GrokSettleStatus::TimedOut => {
                Some(self.record_grok_teardown_unavailable(execution_id, session_name, &result.progress, settle_window))
            }
        }
    }

    async fn finish_grok_settle_ingest(
        &self,
        execution_id: &str,
        session_name: &str,
        paths: &GrokTelemetryPaths,
        turn_completed: usize,
        deadline: tokio::time::Instant,
        already_complete: bool,
    ) -> Option<tokio::task::JoinHandle<()>> {
        match timeout_remaining(deadline, self.ingest_grok_updates(execution_id, paths)).await {
            Some(true) => {
                tracing::info!(
                    execution_id,
                    session = session_name,
                    turn_completed,
                    already_complete,
                    "reap_tmux_worker: captured Grok turn_completed before reap",
                );
                None
            }
            Some(false) => {
                tracing::warn!(
                    execution_id,
                    session = session_name,
                    turn_completed,
                    reason = GROK_TEARDOWN_INGEST_FAILED_REASON,
                    "reap_tmux_worker: Grok turn_completed was present but usage capture failed",
                );
                Some(self.spawn_persist_grok_usage_unavailable(execution_id, GROK_TEARDOWN_INGEST_FAILED_REASON))
            }
            None => {
                tracing::warn!(
                    execution_id,
                    session = session_name,
                    turn_completed,
                    reason = GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON,
                    "reap_tmux_worker: teardown bound expired during Grok usage capture; proceeding with reap",
                );
                Some(self.spawn_persist_grok_usage_unavailable(execution_id, GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON))
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
        tokio::task::spawn_blocking(move || {
            let snapshot = RunCostSnapshot {
                usage_unavailable: Some(reason),
                ..Default::default()
            };
            if let Err(err) = work_db.set_run_cost_snapshot(&execution_id, snapshot) {
                tracing::warn!(
                    execution_id,
                    error = %format!("{err:#}"),
                    "reap_tmux_worker: failed recording Grok teardown usage_unavailable reason",
                );
            }
        })
    }

    async fn ingest_grok_updates(&self, execution_id: &str, paths: &GrokTelemetryPaths) -> bool {
        let Some(updates_path) = paths.updates_path.as_deref() else {
            return false;
        };
        let Some(containment_root) = paths.containment_root.as_ref() else {
            return false;
        };
        if !tokio::fs::try_exists(updates_path).await.unwrap_or(false) {
            return false;
        }
        let path_str = updates_path.to_string_lossy().into_owned();
        let work_db = self.work_db.clone();
        let exec_id = execution_id.to_owned();
        let _ =
            tokio::task::spawn_blocking(move || work_db.set_run_transcript_path_if_unset(&exec_id, &path_str)).await;
        match self
            .run_cost_capture
            .capture_and_persist(&self.work_db, execution_id, updates_path, containment_root.as_deref())
            .await
        {
            Ok(persisted) => persisted,
            Err(error) => {
                tracing::warn!(
                    execution_id,
                    transcript_path = %updates_path.display(),
                    %error,
                    "reap_tmux_worker: failed capturing Grok run cost from updates.jsonl",
                );
                false
            }
        }
    }
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

async fn poll_tail(tail: &mut TranscriptTail) -> Vec<Value> {
    match tail.poll().await {
        Ok(values) => values,
        Err(error) => {
            tracing::debug!(
                path = %tail.path().display(),
                %error,
                "reap_tmux_worker: Grok transcript tail poll failed; treating as no new lines",
            );
            Vec::new()
        }
    }
}
