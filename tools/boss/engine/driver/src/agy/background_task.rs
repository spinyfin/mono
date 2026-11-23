//! Live `agy` background-task row classification for hang detection.
//!
//! Classifies raw on-disk `agy` transcript rows (`GENERIC` `RUNNING` and
//! `SYSTEM_MESSAGE`) into active background-task correlation state. File
//! tailing, containment, and the unmatched-`RUNNING` horizon live in
//! `boss_engine_completion` on top of `boss_transcript_tail`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{is_model_sourced, status_of, text_of};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveTask {
    started_epoch: i64,
    log_path: Option<PathBuf>,
    /// Launched by a `run_command` with `IsDaemon: true`. The model does not
    /// wait on a daemon, so it never counts as a blocking background wait.
    daemon: bool,
}

/// Retained correlation state for one execution's `agy` transcript.
#[derive(Debug, Default)]
pub struct BackgroundTaskCorrelation {
    active_tasks: HashMap<String, ActiveTask>,
    anonymous_running_epochs: VecDeque<i64>,
    /// `IsDaemon` flags of the `run_command` calls in the latest planner
    /// response, consumed in order by the `RUNNING` rows they launch.
    pending_launches: VecDeque<bool>,
}

impl BackgroundTaskCorrelation {
    pub fn reset(&mut self) {
        self.active_tasks.clear();
        self.anonymous_running_epochs.clear();
        self.pending_launches.clear();
    }

    pub fn has_unmatched_running(&self) -> bool {
        !self.active_tasks.is_empty() || !self.anonymous_running_epochs.is_empty()
    }

    /// Whether a non-daemon background task launched after `cutoff_epoch` is
    /// still unmatched by a completion. Each task is judged against the cutoff
    /// on its own, so an orphaned old task neither extends nor revokes the
    /// suppression owed to a newer one.
    pub fn has_blocking_task_since(&self, cutoff_epoch: i64) -> bool {
        self.blocking_tasks_since(cutoff_epoch).next().is_some()
            || self.anonymous_running_epochs.iter().any(|epoch| *epoch > cutoff_epoch)
    }

    fn blocking_tasks_since(&self, cutoff_epoch: i64) -> impl Iterator<Item = &ActiveTask> {
        self.active_tasks
            .values()
            .filter(move |task| !task.daemon && task.started_epoch > cutoff_epoch)
    }

    /// Task log paths recorded on currently active `RUNNING` rows.
    pub fn active_log_paths(&self) -> impl Iterator<Item = &Path> {
        self.active_tasks.values().filter_map(|task| task.log_path.as_deref())
    }

    pub fn apply_line(&mut self, line: &str, now_epoch: i64) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        // Cheap pre-filter: only these row kinds change correlation state, and
        // transcripts carry many large rows that are none of them.
        if !(trimmed.contains("\"GENERIC\"")
            || trimmed.contains("\"SYSTEM_MESSAGE\"")
            || trimmed.contains("\"PLANNER_RESPONSE\""))
        {
            return;
        }
        let Ok(raw) = serde_json::from_str::<Value>(trimmed) else {
            return;
        };
        self.apply_row(&raw, now_epoch);
    }

    pub fn apply_row(&mut self, raw: &Value, now_epoch: i64) {
        let Some(row) = raw.as_object() else {
            return;
        };
        let Some(step_type) = row.get("type").and_then(Value::as_str) else {
            return;
        };
        let row_epoch = row
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(boss_engine_utils::iso8601::parse_iso8601_to_epoch)
            .unwrap_or(now_epoch);
        match step_type {
            "PLANNER_RESPONSE" => {
                if let Some(calls) = row.get("tool_calls").and_then(Value::as_array) {
                    self.pending_launches = calls
                        .iter()
                        .filter(|call| call.get("name").and_then(Value::as_str) == Some("run_command"))
                        .map(|call| call.pointer("/args/IsDaemon").and_then(Value::as_bool).unwrap_or(false))
                        .collect();
                }
            }
            "GENERIC" if is_model_sourced(row) && status_of(row) == "RUNNING" => {
                let content = text_of(row, "content");
                let log_path = extract_task_log_path(content);
                let daemon = self.pending_launches.pop_front().unwrap_or(false);
                if let Some(id) = extract_running_task_id(content) {
                    self.active_tasks.insert(
                        id.to_owned(),
                        ActiveTask {
                            started_epoch: row_epoch,
                            log_path,
                            daemon,
                        },
                    );
                } else if !daemon {
                    self.anonymous_running_epochs.push_back(row_epoch);
                }
            }
            "SYSTEM_MESSAGE" => {
                let content = text_of(row, "content");
                let mut removed = false;
                for id in extract_completed_task_ids(content) {
                    removed |= self.active_tasks.remove(id).is_some();
                }
                if !removed {
                    let finished = content.matches("finished with result:").count();
                    for _ in 0..finished {
                        self.anonymous_running_epochs.pop_front();
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
pub(super) fn reader_has_live_background_task(reader: impl std::io::BufRead) -> bool {
    let mut correlation = BackgroundTaskCorrelation::default();
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        correlation.apply_line(&line, 0);
    }
    correlation.has_unmatched_running()
}

pub fn extract_running_task_id(content: &str) -> Option<&str> {
    let rest = content.split("task id:").nth(1)?;
    let id = rest.split_whitespace().next()?;
    (!id.is_empty()).then_some(id)
}

pub fn extract_task_log_path(content: &str) -> Option<PathBuf> {
    let rest = content.split("Task logs are available at:").nth(1)?;
    let token = rest.split_whitespace().next()?;
    let path = token.strip_prefix("file://").unwrap_or(token);
    (!path.is_empty()).then(|| PathBuf::from(path))
}

pub fn extract_completed_task_ids(content: &str) -> Vec<&str> {
    let mut ids = Vec::new();
    for rest in content.split("sender=").skip(1) {
        if let Some(id) = rest.split_whitespace().next()
            && !id.is_empty()
        {
            ids.push(id);
        }
    }
    let mut remaining = content;
    while let Some(idx) = remaining.find("Task id \"") {
        remaining = &remaining[idx + "Task id \"".len()..];
        if let Some(end) = remaining.find('"') {
            let id = &remaining[..end];
            if !id.is_empty() {
                ids.push(id);
            }
            remaining = &remaining[end + 1..];
        } else {
            break;
        }
    }
    ids
}
