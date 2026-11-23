//! Incremental `agy` background-task scan cache for hang detection.
//!
//! Feeds `boss_engine_driver::agy::BackgroundTaskCorrelation` from
//! `boss_transcript_tail::raw::read_contained_sync`, preserving the shared
//! crate's descriptor-verified containment, partial-line framing, and
//! same-inode truncate-and-regrow detection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use boss_engine_driver::agy::BackgroundTaskCorrelation;
use boss_transcript_tail::raw;

use crate::build_wait_tracker::DEFAULT_BUILD_WAIT_HORIZON_SECS;

/// Bound on a single `read_contained_sync` chunk. Later chunks in the same
/// poll continue from the updated cursor until EOF.
const MAX_TRANSCRIPT_SCAN_BATCH_BYTES: usize = 64 * 1024 * 1024;

/// Whether the worker or background-task process is currently alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskLiveness {
    /// The worker or active background task appears to be running.
    Alive,
    /// The worker or active background task has exited or gone stale.
    Exited,
    /// No pid or task log was recorded; fall back to the unmatched-`RUNNING` horizon.
    Unknown,
}

/// Retained correlation and `boss_transcript_tail` cursor for one execution.
#[derive(Debug, Default)]
struct BackgroundTaskScan {
    path: PathBuf,
    cursor: raw::Cursor,
    partial: Vec<u8>,
    correlation: BackgroundTaskCorrelation,
}

impl BackgroundTaskScan {
    fn reset(&mut self) {
        self.cursor = raw::Cursor::default();
        self.partial.clear();
        self.correlation.reset();
    }
}

/// Per-execution incremental scans so hang snapshots do not re-parse the
/// whole transcript on every call.
#[derive(Debug, Default)]
pub struct BackgroundTaskScanCache {
    scans: Mutex<HashMap<String, BackgroundTaskScan>>,
}

impl BackgroundTaskScanCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn poll_with_probe<F>(
        &self,
        execution_id: &str,
        path: &Path,
        containment_root: Option<&Path>,
        now_epoch: i64,
        probe: F,
    ) -> bool
    where
        F: FnOnce(&[&Path]) -> BackgroundTaskLiveness,
    {
        let mut scans = self.scans.lock().expect("agy background-task scan cache poisoned");
        let scan = scans.entry(execution_id.to_owned()).or_default();
        poll_scan(
            scan,
            path,
            containment_root,
            now_epoch,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            probe,
        )
    }

    pub fn forget(&self, execution_id: &str) {
        self.scans
            .lock()
            .expect("agy background-task scan cache poisoned")
            .remove(execution_id);
    }
}

fn poll_scan<F>(
    scan: &mut BackgroundTaskScan,
    path: &Path,
    containment_root: Option<&Path>,
    now_epoch: i64,
    max_batch_bytes: usize,
    probe: F,
) -> bool
where
    F: FnOnce(&[&Path]) -> BackgroundTaskLiveness,
{
    if !ingest_transcript(scan, path, containment_root, max_batch_bytes, now_epoch) {
        return false;
    }
    if !scan.correlation.has_unmatched_running() {
        return false;
    }
    let Some(oldest_epoch) = scan.correlation.oldest_active_epoch() else {
        return false;
    };
    if now_epoch.saturating_sub(oldest_epoch) >= DEFAULT_BUILD_WAIT_HORIZON_SECS {
        return false;
    }
    let log_paths: Vec<&Path> = scan.correlation.active_log_paths().collect();
    match probe(&log_paths) {
        BackgroundTaskLiveness::Exited => false,
        BackgroundTaskLiveness::Alive | BackgroundTaskLiveness::Unknown => true,
    }
}

fn ingest_transcript(
    scan: &mut BackgroundTaskScan,
    path: &Path,
    containment_root: Option<&Path>,
    max_batch_bytes: usize,
    now_epoch: i64,
) -> bool {
    if scan.path != path {
        scan.reset();
        scan.path = path.to_path_buf();
    }
    let batch_bytes = max_batch_bytes.max(1);
    loop {
        let chunk = match raw::read_contained_sync(path, containment_root, &scan.cursor, batch_bytes) {
            Ok(chunk) => chunk,
            Err(_) => {
                scan.reset();
                return false;
            }
        };
        if chunk.replaced {
            scan.correlation.reset();
            scan.partial.clear();
        }
        scan.cursor = chunk.cursor;
        if !chunk.data.is_empty() {
            apply_bytes(scan, &chunk.data, now_epoch);
        }
        if chunk.eof {
            break;
        }
    }
    true
}

fn apply_bytes(scan: &mut BackgroundTaskScan, data: &[u8], now_epoch: i64) {
    scan.partial.extend_from_slice(data);
    let mut start = 0usize;
    for i in 0..scan.partial.len() {
        if scan.partial[i] == b'\n' {
            let line = String::from_utf8_lossy(&scan.partial[start..i]);
            scan.correlation.apply_line(&line, now_epoch);
            start = i + 1;
        }
    }
    if start > 0 {
        scan.partial.drain(..start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn unmatched_running_is_live_only_while_the_process_is_alive() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"GENERIC","source":"MODEL","status":"RUNNING","content":"Tool is running as a background task with task id: conv/task-1\n"}"#,
                "\n",
            ),
        )
        .unwrap();
        let now = 1_700_000_000;
        let mut live_scan = BackgroundTaskScan::default();
        assert!(poll_scan(
            &mut live_scan,
            &path,
            None,
            now,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));
        let mut exited_scan = BackgroundTaskScan::default();
        assert!(!poll_scan(
            &mut exited_scan,
            &path,
            None,
            now,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Exited,
        ));
    }

    #[test]
    fn unmatched_running_stops_suppressing_after_the_horizon() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"GENERIC","source":"MODEL","status":"RUNNING","created_at":"2024-01-01T00:00:00Z","content":"Tool is running as a background task with task id: conv/task-1\n"}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut scan = BackgroundTaskScan::default();
        let start = boss_engine_utils::iso8601::parse_iso8601_to_epoch("2024-01-01T00:00:00Z").unwrap();
        assert!(poll_scan(
            &mut scan,
            &path,
            None,
            start,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));
        assert!(!poll_scan(
            &mut scan,
            &path,
            None,
            start + DEFAULT_BUILD_WAIT_HORIZON_SECS,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));
    }

    #[test]
    fn incremental_scan_sees_launch_and_completion_on_opposite_sides_of_a_batch() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let padding = format!("{}\n", r#"{"type":"USER_INPUT","status":"DONE","content":"pad"}"#);
        let running = format!(
            "{}\n",
            r#"{"type":"GENERIC","source":"MODEL","status":"RUNNING","content":"Tool is running as a background task with task id: conv/task-1\n"}"#
        );
        let completion = format!(
            "{}\n",
            r#"{"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"[Message] sender=conv/task-1 content=Task id \"conv/task-1\" finished with result:\nok"}"#
        );
        let batch = 48usize;
        assert!(padding.len() > batch, "padding must straddle the batch");
        std::fs::write(&path, format!("{running}{padding}")).unwrap();
        let mut scan = BackgroundTaskScan::default();
        let now = 1_700_000_000;
        assert!(poll_scan(&mut scan, &path, None, now, batch, |_| {
            BackgroundTaskLiveness::Alive
        }));
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(completion.as_bytes()).unwrap();
        drop(file);
        assert!(!poll_scan(&mut scan, &path, None, now, batch, |_| {
            BackgroundTaskLiveness::Alive
        }));
    }

    #[test]
    fn incremental_scan_sees_a_launch_past_the_first_batch() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let padding = format!("{}\n", r#"{"type":"USER_INPUT","status":"DONE","content":"pad"}"#);
        let running = format!(
            "{}\n",
            r#"{"type":"GENERIC","source":"MODEL","status":"RUNNING","content":"Tool is running as a background task with task id: conv/task-1\n"}"#
        );
        let batch = 48usize;
        assert!(padding.len() > batch);
        std::fs::write(&path, format!("{padding}{running}")).unwrap();
        let mut scan = BackgroundTaskScan::default();
        assert!(poll_scan(&mut scan, &path, None, 1_700_000_000, batch, |_| {
            BackgroundTaskLiveness::Alive
        },));
    }

    #[test]
    fn transcript_outside_containment_root_is_not_live() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside.jsonl");
        std::fs::write(
            &outside,
            concat!(
                r#"{"type":"GENERIC","source":"MODEL","status":"RUNNING","content":"Tool is running as a background task with task id: conv/task-1\n"}"#,
                "\n",
            ),
        )
        .unwrap();
        let cache = BackgroundTaskScanCache::new();
        assert!(
            !cache.poll_with_probe("exec-out", &outside, Some(&root), 1_700_000_000, |_| {
                BackgroundTaskLiveness::Alive
            })
        );
        let inside = root.join("inside.jsonl");
        std::fs::copy(&outside, &inside).unwrap();
        assert!(
            cache.poll_with_probe("exec-in", &inside, Some(&root), 1_700_000_000, |_| {
                BackgroundTaskLiveness::Alive
            })
        );
    }

    #[test]
    fn same_inode_truncate_and_regrow_resets_correlation() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let running = concat!(
            r#"{"type":"GENERIC","source":"MODEL","status":"RUNNING","content":"Tool is running as a background task with task id: conv/task-1\n"}"#,
            "\n",
        );
        std::fs::write(&path, running).unwrap();
        let mut scan = BackgroundTaskScan::default();
        let now = 1_700_000_000;
        assert!(poll_scan(
            &mut scan,
            &path,
            None,
            now,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));

        // Rewrite in place (same inode) to a larger file with no RUNNING row.
        let replacement = concat!(
            r#"{"type":"USER_INPUT","status":"DONE","content":"fresh session after truncate and regrow"}"#,
            "\n",
            r#"{"type":"PLANNER_RESPONSE","status":"DONE","content":"no background tasks here at all, longer than before"}"#,
            "\n",
        );
        assert!(replacement.len() >= running.len());
        std::fs::write(&path, replacement).unwrap();

        assert!(!poll_scan(
            &mut scan,
            &path,
            None,
            now,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));
    }

    #[test]
    fn two_poll_partial_running_row_is_applied_once_when_completed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let full_running = concat!(
            r#"{"type":"GENERIC","source":"MODEL","status":"RUNNING","content":"Tool is running as a background task with task id: conv/task-1\n"}"#,
            "\n",
        );
        let split = full_running.len() / 2;
        std::fs::write(&path, &full_running[..split]).unwrap();

        let mut scan = BackgroundTaskScan::default();
        let now = 1_700_000_000;
        assert!(!poll_scan(
            &mut scan,
            &path,
            None,
            now,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&full_running.as_bytes()[split..]).unwrap();
        drop(file);

        assert!(poll_scan(
            &mut scan,
            &path,
            None,
            now,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));
    }

    #[test]
    fn two_poll_partial_completion_row_is_applied_once_when_completed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let running = concat!(
            r#"{"type":"GENERIC","source":"MODEL","status":"RUNNING","content":"Tool is running as a background task with task id: conv/task-1\n"}"#,
            "\n",
        );
        let completion = concat!(
            r#"{"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"[Message] sender=conv/task-1 content=Task id \"conv/task-1\" finished with result:\nok"}"#,
            "\n",
        );
        let split = completion.len() / 2;
        std::fs::write(&path, format!("{running}{}", &completion[..split])).unwrap();

        let mut scan = BackgroundTaskScan::default();
        let now = 1_700_000_000;
        assert!(poll_scan(
            &mut scan,
            &path,
            None,
            now,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&completion.as_bytes()[split..]).unwrap();
        drop(file);

        assert!(!poll_scan(
            &mut scan,
            &path,
            None,
            now,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));
    }

    #[test]
    fn old_completed_task_followed_by_fresh_live_task_uses_fresh_horizon() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let t0 = "2024-01-01T00:00:00Z";
        let t_fresh = "2024-01-01T00:40:00Z";
        let t0_epoch = boss_engine_utils::iso8601::parse_iso8601_to_epoch(t0).unwrap();
        let t_fresh_epoch = boss_engine_utils::iso8601::parse_iso8601_to_epoch(t_fresh).unwrap();
        let transcript = format!(
            "{}\n{}\n{}\n",
            format_args!(
                r#"{{"type":"GENERIC","source":"MODEL","status":"RUNNING","created_at":"{t0}","content":"Tool is running as a background task with task id: conv/task-old\n"}}"#
            ),
            r#"{"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"[Message] sender=conv/task-old content=Task id \"conv/task-old\" finished with result:\nok"}"#,
            format_args!(
                r#"{{"type":"GENERIC","source":"MODEL","status":"RUNNING","created_at":"{t_fresh}","content":"Tool is running as a background task with task id: conv/task-new\n"}}"#
            ),
        );
        std::fs::write(&path, transcript).unwrap();

        let mut scan = BackgroundTaskScan::default();
        // Past old task's horizon (t0 + 2700), well within fresh task's horizon.
        let now = t0_epoch + DEFAULT_BUILD_WAIT_HORIZON_SECS + 60;
        assert!(now < t_fresh_epoch + DEFAULT_BUILD_WAIT_HORIZON_SECS);
        assert!(poll_scan(
            &mut scan,
            &path,
            None,
            now,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));
    }

    #[test]
    fn overlapping_tasks_anchor_horizon_on_oldest_remaining_active_task() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let t0 = "2024-01-01T00:00:00Z";
        let t_second = "2024-01-01T00:40:00Z";
        let t0_epoch = boss_engine_utils::iso8601::parse_iso8601_to_epoch(t0).unwrap();
        let t_second_epoch = boss_engine_utils::iso8601::parse_iso8601_to_epoch(t_second).unwrap();
        let transcript = format!(
            "{}\n{}\n{}\n",
            format_args!(
                r#"{{"type":"GENERIC","source":"MODEL","status":"RUNNING","created_at":"{t0}","content":"Tool is running as a background task with task id: conv/task-1\n"}}"#
            ),
            format_args!(
                r#"{{"type":"GENERIC","source":"MODEL","status":"RUNNING","created_at":"{t_second}","content":"Tool is running as a background task with task id: conv/task-2\n"}}"#
            ),
            r#"{"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"[Message] sender=conv/task-1 content=Task id \"conv/task-1\" finished with result:\nok"}"#,
        );
        std::fs::write(&path, transcript).unwrap();

        let mut scan = BackgroundTaskScan::default();
        let still_within_second = t0_epoch + DEFAULT_BUILD_WAIT_HORIZON_SECS + 60;
        assert!(still_within_second < t_second_epoch + DEFAULT_BUILD_WAIT_HORIZON_SECS);
        assert!(poll_scan(
            &mut scan,
            &path,
            None,
            still_within_second,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));
        assert!(!poll_scan(
            &mut scan,
            &path,
            None,
            t_second_epoch + DEFAULT_BUILD_WAIT_HORIZON_SECS,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
            |_| BackgroundTaskLiveness::Alive,
        ));
    }
}
