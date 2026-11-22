//! Live `agy` background-task correlation for hang detection.
//!
//! A `GENERIC` `RUNNING` row (or its canonical `tool_running` form) counts as
//! a blocking tool poll only while the unmatched task is still live: the
//! worker/task process is alive, and a horizon bound resumes detection if a
//! completion notice never arrives. Transcripts are ingested incrementally
//! with retained correlation, following the same offset / truncation /
//! replacement contract as [`boss_transcript_tail`]: each read is bounded,
//! and later polls resume from the last scanned offset rather than rereading
//! a lifetime prefix.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::Value;

use super::{is_model_sourced, status_of, text_of};

/// Bound on a single ingest read. Later polls continue from the retained
/// offset, so records past this size are visible instead of being dropped.
pub const MAX_TRANSCRIPT_SCAN_BATCH_BYTES: u64 = 64 * 1024 * 1024;

/// How long an unmatched `RUNNING` row may suppress hang detection while its
/// process still looks alive. Matches the delegated-work horizon used for
/// background children (45 minutes): long enough for a real `gh pr checks
/// --watch`, short enough that a wedged command still surfaces.
pub const LIVE_BACKGROUND_TASK_HORIZON_SECS: i64 = 45 * 60;

/// Whether the worker or background-task process is currently alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskLiveness {
    /// The worker/task process is running.
    Alive,
    /// The worker/task process has exited, been killed, or belongs to a
    /// crashed or restarted session.
    Exited,
    /// No pid was recorded; fall back to the unmatched-RUNNING horizon.
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
    len: u64,
    mtime: Option<SystemTime>,
    device: u64,
    inode: u64,
}

#[derive(Debug, Default)]
struct ScanCursor {
    path: PathBuf,
    offset: u64,
    stamp: Option<FileStamp>,
    partial: String,
}

/// Retained correlation and read cursor for one execution's transcript.
#[derive(Debug, Default)]
pub struct BackgroundTaskScan {
    cursor: ScanCursor,
    active_task_ids: HashSet<String>,
    anonymous_running: usize,
    first_unmatched_epoch: Option<i64>,
}

impl BackgroundTaskScan {
    pub fn has_unmatched_running(&self) -> bool {
        !self.active_task_ids.is_empty() || self.anonymous_running > 0
    }

    fn reset_correlation(&mut self) {
        self.cursor = ScanCursor::default();
        self.active_task_ids.clear();
        self.anonymous_running = 0;
        self.first_unmatched_epoch = None;
    }
}

/// Per-execution incremental scans so hang snapshots do not re-parse the
/// whole transcript or re-resolve the driver on every call.
#[derive(Debug, Default)]
pub struct BackgroundTaskScanCache {
    scans: Mutex<HashMap<String, BackgroundTaskScan>>,
    forced_liveness: Mutex<HashMap<String, BackgroundTaskLiveness>>,
}

impl BackgroundTaskScanCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn poll(
        &self,
        execution_id: &str,
        path: &Path,
        containment_root: Option<&Path>,
        liveness: BackgroundTaskLiveness,
        now_epoch: i64,
    ) -> bool {
        self.poll_with_batch(
            execution_id,
            path,
            containment_root,
            liveness,
            now_epoch,
            MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
        )
    }

    pub fn poll_with_batch(
        &self,
        execution_id: &str,
        path: &Path,
        containment_root: Option<&Path>,
        liveness: BackgroundTaskLiveness,
        now_epoch: i64,
        max_batch_bytes: u64,
    ) -> bool {
        let mut scans = self.scans.lock().expect("agy background-task scan cache poisoned");
        let scan = scans.entry(execution_id.to_owned()).or_default();
        poll_live_background_task_with_batch(scan, path, containment_root, liveness, now_epoch, max_batch_bytes)
    }

    pub fn forget(&self, execution_id: &str) {
        self.scans
            .lock()
            .expect("agy background-task scan cache poisoned")
            .remove(execution_id);
        self.forced_liveness
            .lock()
            .expect("agy background-task scan cache poisoned")
            .remove(execution_id);
    }

    pub fn force_liveness(&self, execution_id: &str, liveness: BackgroundTaskLiveness) {
        self.forced_liveness
            .lock()
            .expect("agy background-task scan cache poisoned")
            .insert(execution_id.to_owned(), liveness);
    }

    pub fn forced_liveness(&self, execution_id: &str) -> Option<BackgroundTaskLiveness> {
        self.forced_liveness
            .lock()
            .expect("agy background-task scan cache poisoned")
            .get(execution_id)
            .copied()
    }
}

/// One-shot helper for callers that hold no retained scan state.
pub fn transcript_has_live_background_task(path: &Path, containment_root: Option<&Path>) -> bool {
    let mut scan = BackgroundTaskScan::default();
    poll_live_background_task(
        &mut scan,
        path,
        containment_root,
        BackgroundTaskLiveness::Unknown,
        boss_engine_utils::epoch_time::now_epoch_secs(),
    )
}

pub fn poll_live_background_task(
    scan: &mut BackgroundTaskScan,
    path: &Path,
    containment_root: Option<&Path>,
    liveness: BackgroundTaskLiveness,
    now_epoch: i64,
) -> bool {
    poll_live_background_task_with_batch(
        scan,
        path,
        containment_root,
        liveness,
        now_epoch,
        MAX_TRANSCRIPT_SCAN_BATCH_BYTES,
    )
}

pub fn poll_live_background_task_with_batch(
    scan: &mut BackgroundTaskScan,
    path: &Path,
    containment_root: Option<&Path>,
    liveness: BackgroundTaskLiveness,
    now_epoch: i64,
    max_batch_bytes: u64,
) -> bool {
    if !ingest_transcript(scan, path, containment_root, max_batch_bytes, now_epoch) {
        return false;
    }
    decide_live(scan, liveness, now_epoch)
}

fn decide_live(scan: &mut BackgroundTaskScan, liveness: BackgroundTaskLiveness, now_epoch: i64) -> bool {
    if !scan.has_unmatched_running() {
        scan.first_unmatched_epoch = None;
        return false;
    }
    if scan.first_unmatched_epoch.is_none() {
        scan.first_unmatched_epoch = Some(now_epoch);
    }
    let age = now_epoch.saturating_sub(scan.first_unmatched_epoch.unwrap_or(now_epoch));
    if age >= LIVE_BACKGROUND_TASK_HORIZON_SECS {
        return false;
    }
    match liveness {
        BackgroundTaskLiveness::Exited => false,
        BackgroundTaskLiveness::Alive | BackgroundTaskLiveness::Unknown => true,
    }
}

fn ingest_transcript(
    scan: &mut BackgroundTaskScan,
    path: &Path,
    containment_root: Option<&Path>,
    max_batch_bytes: u64,
    now_epoch: i64,
) -> bool {
    if let Some(root) = containment_root
        && !path_is_contained(path, root)
    {
        scan.reset_correlation();
        return false;
    }
    let Ok(mut file) = open_transcript(path) else {
        scan.reset_correlation();
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        scan.reset_correlation();
        return false;
    };
    if !metadata.is_file() {
        scan.reset_correlation();
        return false;
    }
    let stamp = file_stamp(&metadata);
    if scan.cursor.path != path {
        scan.reset_correlation();
        scan.cursor.path = path.to_path_buf();
    }
    let replaced = scan.cursor.stamp.is_some_and(|prior| {
        (prior.device != 0 || prior.inode != 0) && (prior.device != stamp.device || prior.inode != stamp.inode)
    });
    if replaced || stamp.len < scan.cursor.offset {
        scan.reset_correlation();
        scan.cursor.path = path.to_path_buf();
    } else if scan.cursor.stamp == Some(stamp) && scan.cursor.offset == stamp.len && scan.cursor.partial.is_empty() {
        return true;
    }
    scan.cursor.stamp = Some(stamp);

    loop {
        let remaining = stamp.len.saturating_sub(scan.cursor.offset);
        if remaining == 0 {
            break;
        }
        let to_read = remaining.min(max_batch_bytes.max(1));
        if file.seek(SeekFrom::Start(scan.cursor.offset)).is_err() {
            scan.reset_correlation();
            return false;
        }
        let mut buf = vec![0; to_read as usize];
        let Ok(n) = file.read(&mut buf) else {
            scan.reset_correlation();
            return false;
        };
        if n == 0 {
            break;
        }
        buf.truncate(n);
        scan.cursor.offset += n as u64;
        apply_bytes(scan, &buf, now_epoch);
        if (n as u64) < to_read {
            break;
        }
    }
    if scan.cursor.offset >= stamp.len && !scan.cursor.partial.is_empty() {
        let leftover = std::mem::take(&mut scan.cursor.partial);
        apply_line(scan, &leftover, now_epoch);
    }
    true
}

fn apply_bytes(scan: &mut BackgroundTaskScan, buf: &[u8], now_epoch: i64) {
    let chunk = String::from_utf8_lossy(buf);
    let mut combined = std::mem::take(&mut scan.cursor.partial);
    combined.push_str(&chunk);
    let mut start = 0usize;
    let bytes = combined.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let line = &combined[start..i];
            start = i + 1;
            apply_line(scan, line, now_epoch);
        }
    }
    scan.cursor.partial = combined[start..].to_owned();
}

fn apply_line(scan: &mut BackgroundTaskScan, line: &str, now_epoch: i64) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let Ok(raw) = serde_json::from_str::<Value>(trimmed) else {
        return;
    };
    apply_row(scan, &raw, now_epoch);
}

fn apply_row(scan: &mut BackgroundTaskScan, raw: &Value, now_epoch: i64) {
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
        "GENERIC" if is_model_sourced(row) && status_of(row) == "RUNNING" => {
            let content = text_of(row, "content");
            if let Some(id) = extract_running_task_id(content) {
                scan.active_task_ids.insert(id.to_owned());
            } else {
                scan.anonymous_running += 1;
            }
            note_unmatched(scan, row_epoch);
        }
        "SYSTEM_MESSAGE" => {
            let content = text_of(row, "content");
            let mut removed = false;
            for id in extract_completed_task_ids(content) {
                removed |= scan.active_task_ids.remove(id);
            }
            if !removed {
                let finished = content.matches("finished with result:").count();
                scan.anonymous_running = scan.anonymous_running.saturating_sub(finished);
            }
        }
        "system" => match row.get("subtype").and_then(Value::as_str) {
            Some("tool_running") => {
                scan.anonymous_running += 1;
                note_unmatched(scan, row_epoch);
            }
            Some("system_message") => {
                scan.anonymous_running = scan.anonymous_running.saturating_sub(1);
            }
            _ => {}
        },
        _ => {}
    }
}

fn note_unmatched(scan: &mut BackgroundTaskScan, epoch: i64) {
    scan.first_unmatched_epoch = Some(scan.first_unmatched_epoch.map_or(epoch, |prior| prior.min(epoch)));
}

/// Whether `reader` shows at least one unmatched background task.
pub fn reader_has_live_background_task(reader: impl BufRead) -> bool {
    let mut scan = BackgroundTaskScan::default();
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        apply_line(&mut scan, &line, 0);
    }
    scan.has_unmatched_running()
}

fn extract_running_task_id(content: &str) -> Option<&str> {
    let rest = content.split("task id:").nth(1)?;
    let id = rest.split_whitespace().next()?;
    (!id.is_empty()).then_some(id)
}

fn extract_completed_task_ids(content: &str) -> Vec<&str> {
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

fn path_is_contained(path: &Path, root: &Path) -> bool {
    let (Ok(canon_root), Ok(canon_path)) = (std::fs::canonicalize(root), std::fs::canonicalize(path)) else {
        return false;
    };
    canon_path.starts_with(&canon_root)
}

fn open_transcript(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn file_stamp(metadata: &std::fs::Metadata) -> FileStamp {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileStamp {
            len: metadata.len(),
            mtime: metadata.modified().ok(),
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        FileStamp {
            len: metadata.len(),
            mtime: metadata.modified().ok(),
            device: 0,
            inode: 0,
        }
    }
}
