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
//! including the initial reads, the interrupt send, polling, and the final
//! ingest.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use boss_engine_driver::{AgentDriver, InterruptPlan};
use boss_engine_runtime::transcript_tail::TranscriptTail;
use boss_engine_storage_values::RunCostSnapshot;
use boss_tmux::Tmux;
use serde_json::Value;

use super::*;

/// Upper bound on waiting for Grok to emit `turn_completed` after cancelling an
/// open turn before teardown proceeds. Kept short so slot and lease release
/// stay prompt even if Grok never responds.
pub(super) const GROK_TEARDOWN_SETTLE_WINDOW: Duration = Duration::from_secs(5);

const GROK_TEARDOWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Specific `usage_unavailable` reason persisted when a Grok turn is still
/// missing `turn_completed` after the pre-reap settle bound expires.
pub(crate) const GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON: &str = "turn ended by teardown before turn_completed";

/// Specific `usage_unavailable` reason when `turn_completed` is present but
/// [`RunCostCapture`] could not persist it.
pub(crate) const GROK_TEARDOWN_INGEST_FAILED_REASON: &str = "turn_completed observed but usage capture failed";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct EventsTurnState {
    started: usize,
    ended: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct UpdatesTurnState {
    completed: usize,
    has_activity: bool,
}

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
    events_state: EventsTurnState,
    updates_state: UpdatesTurnState,
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
            events_state: EventsTurnState::default(),
            updates_state: UpdatesTurnState::default(),
        }
    }

    async fn poll(&mut self) {
        if let Some(tail) = self.events.tail.as_mut() {
            let values = poll_tail(tail).await;
            if tail.generation() != self.events.generation {
                self.events_state = EventsTurnState::default();
                self.events.generation = tail.generation();
            }
            fold_events(&mut self.events_state, values);
        }
        if let Some(tail) = self.updates.tail.as_mut() {
            let values = poll_tail(tail).await;
            if tail.generation() != self.updates.generation {
                self.updates_state = UpdatesTurnState::default();
                self.updates.generation = tail.generation();
            }
            fold_updates(&mut self.updates_state, values);
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
        let paths = self.resolve_grok_telemetry_paths(execution_id, driver.as_ref());
        let _ = self.ingest_grok_updates(execution_id, &paths).await;
    }

    /// Ensure an open Grok ACP turn is given a chance to close (via the
    /// driver's interrupt plan / `session/cancel`) and emit `turn_completed`
    /// before `kill_session_verified` destroys the session.
    pub(super) async fn settle_grok_turn_before_reap(&self, tmux: &Tmux, execution_id: &str, session_name: &str) {
        let settle_window = self.grok_teardown_settle_window();
        let deadline = tokio::time::Instant::now() + settle_window;
        let Some(driver) = self.grok_driver_for_execution(execution_id) else {
            return;
        };
        let paths = self.resolve_grok_telemetry_paths(execution_id, driver.as_ref());
        let mut tails = GrokSettleTails::new(&paths);
        if timeout_remaining(deadline, tails.poll()).await.is_none() {
            self.record_grok_teardown_unavailable(execution_id, session_name, &tails, settle_window);
            return;
        }

        if tails.events_state.started == 0 && !tails.updates_state.has_activity {
            return;
        }

        let expected_completed = tails.events_state.started.max(1);
        if tails.updates_state.completed >= expected_completed {
            self.finish_grok_settle_ingest(
                execution_id,
                session_name,
                &paths,
                tails.updates_state.completed,
                deadline,
                true,
            )
            .await;
            return;
        }

        let turn_still_open = tails.events_state.started > tails.events_state.ended
            || (tails.events_state.started == 0 && tails.updates_state.has_activity);
        let mut cancel_delivered = true;
        if turn_still_open {
            tracing::info!(
                execution_id,
                session = session_name,
                turn_started = tails.events_state.started,
                turn_ended = tails.events_state.ended,
                turn_completed = tails.updates_state.completed,
                "reap_tmux_worker: cancelling open Grok ACP turn before reap",
            );
            match timeout_remaining(
                deadline,
                send_interrupt_gesture(tmux, session_name, driver.interrupt_plan()),
            )
            .await
            {
                Some(Ok(())) => {}
                Some(Err(err)) => {
                    cancel_delivered = false;
                    tracing::warn!(
                        execution_id,
                        session = session_name,
                        error = %format!("{err:#}"),
                        "reap_tmux_worker: failed sending interrupt to Grok session before reap; skipping settle wait",
                    );
                }
                None => {
                    self.record_grok_teardown_unavailable(execution_id, session_name, &tails, settle_window);
                    return;
                }
            }
        }

        if cancel_delivered {
            loop {
                if remaining_budget(deadline).is_zero() {
                    break;
                }
                if timeout_remaining(deadline, tails.poll()).await.is_none() {
                    break;
                }
                if tails.updates_state.completed >= expected_completed {
                    self.finish_grok_settle_ingest(
                        execution_id,
                        session_name,
                        &paths,
                        tails.updates_state.completed,
                        deadline,
                        false,
                    )
                    .await;
                    return;
                }
                let sleep_for = GROK_TEARDOWN_POLL_INTERVAL.min(remaining_budget(deadline));
                if sleep_for.is_zero() {
                    break;
                }
                tokio::time::sleep(sleep_for).await;
            }
        }

        if timeout_remaining(deadline, tails.poll()).await.is_some()
            && tails.updates_state.completed >= expected_completed
        {
            self.finish_grok_settle_ingest(
                execution_id,
                session_name,
                &paths,
                tails.updates_state.completed,
                deadline,
                false,
            )
            .await;
            return;
        }

        self.record_grok_teardown_unavailable(execution_id, session_name, &tails, settle_window);
    }

    async fn finish_grok_settle_ingest(
        &self,
        execution_id: &str,
        session_name: &str,
        paths: &GrokTelemetryPaths,
        turn_completed: usize,
        deadline: tokio::time::Instant,
        already_complete: bool,
    ) {
        let ingested = timeout_remaining(deadline, self.ingest_grok_updates(execution_id, paths))
            .await
            .unwrap_or(false);
        if ingested {
            tracing::info!(
                execution_id,
                session = session_name,
                turn_completed,
                already_complete,
                "reap_tmux_worker: captured Grok turn_completed before reap",
            );
            return;
        }
        tracing::warn!(
            execution_id,
            session = session_name,
            turn_completed,
            reason = GROK_TEARDOWN_INGEST_FAILED_REASON,
            "reap_tmux_worker: Grok turn_completed was present but usage capture failed",
        );
        self.persist_grok_usage_unavailable(execution_id, GROK_TEARDOWN_INGEST_FAILED_REASON);
    }

    fn record_grok_teardown_unavailable(
        &self,
        execution_id: &str,
        session_name: &str,
        tails: &GrokSettleTails,
        settle_window: Duration,
    ) {
        self.persist_grok_usage_unavailable(execution_id, GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON);
        tracing::warn!(
            execution_id,
            session = session_name,
            turn_started = tails.events_state.started,
            turn_ended = tails.events_state.ended,
            turn_completed = tails.updates_state.completed,
            settle_window_secs = settle_window.as_secs_f64(),
            reason = GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON,
            "reap_tmux_worker: Grok turn_completed did not arrive before teardown bound; proceeding with reap",
        );
    }

    fn persist_grok_usage_unavailable(&self, execution_id: &str, reason: &str) {
        let snapshot = RunCostSnapshot {
            usage_unavailable: Some(reason.to_owned()),
            ..Default::default()
        };
        if let Err(err) = self.work_db.set_run_cost_snapshot(execution_id, snapshot) {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "reap_tmux_worker: failed recording Grok teardown usage_unavailable reason",
            );
        }
    }

    fn resolve_grok_telemetry_paths(&self, execution_id: &str, driver: &dyn AgentDriver) -> GrokTelemetryPaths {
        let session_files = boss_engine_driver::grok::session_files_for_run(execution_id);
        let updates_path = self
            .transcript_path_cache
            .get(execution_id)
            .map(PathBuf::from)
            .or_else(|| {
                self.work_db
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
        let path_str = updates_path.to_string_lossy();
        let _ = self.work_db.set_run_transcript_path_if_unset(execution_id, &path_str);
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

async fn send_interrupt_gesture(tmux: &Tmux, session_name: &str, plan: Option<InterruptPlan>) -> anyhow::Result<()> {
    let key = plan.map(|p| p.gesture.key).unwrap_or("Escape");
    let presses = plan.map(|p| p.gesture.presses.max(1)).unwrap_or(1);
    let press_interval = plan.map(|p| p.gesture.press_interval).unwrap_or(Duration::ZERO);
    for press in 0..presses {
        if press > 0 && !press_interval.is_zero() {
            tokio::time::sleep(press_interval).await;
        }
        tmux.send_key(session_name, key).await?;
    }
    Ok(())
}

fn remaining_budget(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}

async fn timeout_remaining<F, T>(deadline: tokio::time::Instant, fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    let budget = remaining_budget(deadline);
    if budget.is_zero() {
        return None;
    }
    tokio::time::timeout(budget, fut).await.ok()
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

fn fold_events(state: &mut EventsTurnState, values: Vec<Value>) {
    for value in values {
        if boss_engine_driver::grok::is_turn_started(&value) {
            state.started += 1;
        } else if boss_engine_driver::grok::is_turn_end(&value) {
            state.ended += 1;
        }
    }
}

fn fold_updates(state: &mut UpdatesTurnState, values: Vec<Value>) {
    for value in values {
        state.has_activity = true;
        if boss_engine_driver::grok::is_turn_completed(&value) {
            state.completed += 1;
        }
    }
}
