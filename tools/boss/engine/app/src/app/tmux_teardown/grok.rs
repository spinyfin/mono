//! Pre-reap ACP turn settlement and token capture for Grok tmux workers.
//!
//! A Grok worker runs its task inside one ACP prompt turn. When a completion
//! proposal (`boss propose done`) or another engine-initiated teardown lands
//! while that turn is still open, killing the tmux session immediately
//! prevents Grok from emitting its ACP `turn_completed` record (and usage
//! totals) to `updates.jsonl`.
//!
//! Before destroying a verified-live Grok session, this module checks
//! `events.jsonl` and `updates.jsonl`, sends `Escape` (ACP `session/cancel`) if
//! a turn is still open, waits up to a short bounded window for
//! `turn_completed` to land and be ingested by [`RunCostCapture`], and records
//! a specific `usage_unavailable` reason if the bound expires without
//! `turn_completed`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use boss_engine_driver::AgentDriver;
use boss_engine_runtime::run_cost::is_grok_turn_completed_record;
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

impl ServerState {
    /// Returns whether `execution_id` was launched (or resolves) as a Grok
    /// worker. Uses the launch stamp first and falls back to live resolution
    /// only when unstamped, mirroring `driver_teardown`.
    pub(super) fn is_grok_execution(&self, execution_id: &str) -> bool {
        let slug = match self.work_db.launched_driver_slug(execution_id) {
            Ok(Some(slug)) if !slug.trim().is_empty() => Some(slug),
            Ok(_) => self.work_db.get_execution_driver_slug(execution_id).ok().flatten(),
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    error = %format!("{err:#}"),
                    "reap_tmux_worker: launched-driver lookup failed; falling back to live resolution",
                );
                self.work_db.get_execution_driver_slug(execution_id).ok().flatten()
            }
        };
        slug.as_deref() == Some(boss_engine_driver::grok::TRANSCRIPT_DRIVER_SLUG)
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
        let paths = self.resolve_grok_telemetry_paths(execution_id);
        let _ = self.ingest_grok_updates(execution_id, &paths).await;
    }

    /// Ensure an open Grok ACP turn is given a chance to close (via `Escape` /
    /// `session/cancel`) and emit `turn_completed` before `kill_session_verified`
    /// destroys the session.
    pub(super) async fn settle_grok_turn_before_reap(&self, tmux: &Tmux, execution_id: &str, session_name: &str) {
        let paths = self.resolve_grok_telemetry_paths(execution_id);
        let events_state = match paths.events_path.as_deref() {
            Some(path) => read_events_turn_state(path).await,
            None => EventsTurnState::default(),
        };
        let mut updates_state = match paths.updates_path.as_deref() {
            Some(path) => read_updates_turn_state(path).await,
            None => UpdatesTurnState::default(),
        };

        if events_state.started == 0 && !updates_state.has_activity {
            return;
        }

        let expected_completed = events_state.started.max(1);
        if updates_state.completed >= expected_completed {
            let _ = self.ingest_grok_updates(execution_id, &paths).await;
            return;
        }

        let turn_still_open =
            events_state.started > events_state.ended || (events_state.started == 0 && updates_state.has_activity);
        let mut cancel_delivered = true;
        if turn_still_open {
            tracing::info!(
                execution_id,
                session = session_name,
                turn_started = events_state.started,
                turn_ended = events_state.ended,
                turn_completed = updates_state.completed,
                "reap_tmux_worker: cancelling open Grok ACP turn with Escape before reap",
            );
            if let Err(err) = tmux.send_key(session_name, "Escape").await {
                cancel_delivered = false;
                tracing::warn!(
                    execution_id,
                    session = session_name,
                    error = %format!("{err:#}"),
                    "reap_tmux_worker: failed sending Escape to Grok session before reap; skipping settle wait",
                );
            }
        }

        let settle_window = self.grok_teardown_settle_window();
        let started = Instant::now();
        if cancel_delivered {
            let deadline = tokio::time::Instant::now() + settle_window;
            loop {
                if let Some(path) = paths.updates_path.as_deref() {
                    updates_state = read_updates_turn_state(path).await;
                    if updates_state.completed >= expected_completed {
                        let _ = self.ingest_grok_updates(execution_id, &paths).await;
                        tracing::info!(
                            execution_id,
                            session = session_name,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            turn_completed = updates_state.completed,
                            "reap_tmux_worker: captured Grok turn_completed before reap",
                        );
                        return;
                    }
                }
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                tokio::time::sleep(GROK_TEARDOWN_POLL_INTERVAL.min(deadline - now)).await;
            }
        }

        // One final check in case `turn_completed` landed right at the deadline.
        if let Some(path) = paths.updates_path.as_deref() {
            updates_state = read_updates_turn_state(path).await;
            let _ = self.ingest_grok_updates(execution_id, &paths).await;
            if updates_state.completed >= expected_completed {
                tracing::info!(
                    execution_id,
                    session = session_name,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    turn_completed = updates_state.completed,
                    "reap_tmux_worker: captured Grok turn_completed at teardown bound",
                );
                return;
            }
        }

        let snapshot = RunCostSnapshot {
            usage_unavailable: Some(GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON.to_owned()),
            ..Default::default()
        };
        if let Err(err) = self.work_db.set_run_cost_snapshot(execution_id, snapshot) {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "reap_tmux_worker: failed recording Grok teardown usage_unavailable reason",
            );
        }
        tracing::warn!(
            execution_id,
            session = session_name,
            turn_started = events_state.started,
            turn_ended = events_state.ended,
            turn_completed = updates_state.completed,
            settle_window_secs = settle_window.as_secs_f64(),
            reason = GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON,
            "reap_tmux_worker: Grok turn_completed did not arrive before teardown bound; proceeding with reap",
        );
    }

    fn resolve_grok_telemetry_paths(&self, execution_id: &str) -> GrokTelemetryPaths {
        let session_files = boss_engine_driver::grok::turn_end_recovery::session_files_for_run(execution_id);
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
        let containment_root = match boss_engine_driver::GrokDriver::default().transcript_containment_root(execution_id)
        {
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

async fn read_complete_json_lines(path: &Path) -> Vec<Value> {
    let Ok(bytes) = tokio::fs::read(path).await else {
        return Vec::new();
    };
    let Some(last_newline) = bytes.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    let complete = &bytes[..=last_newline];
    String::from_utf8_lossy(complete)
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then(|| serde_json::from_str::<Value>(trimmed).ok())?
        })
        .collect()
}

async fn read_events_turn_state(path: &Path) -> EventsTurnState {
    let mut state = EventsTurnState::default();
    for value in read_complete_json_lines(path).await {
        if boss_engine_driver::grok::turn_end_recovery::is_turn_started(&value) {
            state.started += 1;
        } else if boss_engine_driver::grok::turn_end_recovery::is_turn_end(&value) {
            state.ended += 1;
        }
    }
    state
}

async fn read_updates_turn_state(path: &Path) -> UpdatesTurnState {
    let mut state = UpdatesTurnState::default();
    for value in read_complete_json_lines(path).await {
        state.has_activity = true;
        if is_grok_turn_completed_record(&value) {
            state.completed += 1;
        }
    }
    state
}
