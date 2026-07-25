//! Incremental, in-memory index of live dispatch timelines.
//!
//! ## Why this exists
//!
//! The stage-stalled detector and the persistent-stall escalation sweep both
//! used to answer "which timelines are stuck?" by walking **every**
//! `executions/<id>/dispatch.jsonl` mirror and fully parsing each one, every
//! pass — the detector every 15s, the escalation sweep every 60s. With
//! 16,187 execution directories holding ~103 MB of records that is a
//! complete rescan of all dispatch history, twice a minute, forever. A 60s
//! engine sample (2026-07-24) put `spawn_stage_stalled_detector` at 7,829
//! samples on one tokio worker, almost all of it inside `read_jsonl` +
//! `serde_json`, with ~10k `stat` / ~9.7k `read` / ~2.5k `open` at the top of
//! stack.
//!
//! The mismatch is that the *scan* window was unbounded while the *decision*
//! window is seconds to minutes. Nothing older than the current pass can
//! change unless a new record is appended for it.
//!
//! ## What it does instead
//!
//! The flat `dispatch-events/current.jsonl` stream is an append-only
//! superset of every mirror (the sink writes it first, then the per-execution
//! copy). So this index reads it **once** at startup, reduces each execution
//! to a fixed-size [`TimelineState`], and thereafter reads only the bytes
//! appended since the previous pass, tracked by a byte offset plus the
//! `(dev, ino)` identity of the file the offset refers to. Steady-state cost
//! per pass is O(bytes appended), independent of how much history exists.
//!
//! Three properties of the fold make that sound (all pinned by tests in
//! [`crate::timeline`]):
//!
//! - it is **idempotent**, so re-reading an overlapping byte range after a
//!   re-anchor cannot corrupt state;
//! - its decisions are **suffix-determined**, so an execution can be dropped
//!   once it goes terminal and rebuilt from a single later record if one ever
//!   arrives — which is what bounds memory to live timelines, not all history;
//! - a record is only consumed once its terminating newline is on disk, so
//!   the torn writes the corpus already contains (the sink appends the record
//!   and its newline as two separate writes) are re-read whole on the next
//!   pass rather than being permanently skipped.
//!
//! ## What it deliberately does not do
//!
//! This index is engine-held state. `bossctl dispatch tail|diagnose|
//! ghost-active|stats` — whose entire value is working when the engine is
//! wedged — keep going through the pure file-scan functions in [`crate`],
//! which are untouched. [`crate::pending_stalls`] and
//! [`crate::persistently_stalled`] likewise remain available as a
//! file-scan-only path, and are asserted to agree with this index.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};

use boss_dispatch_events::DispatchEvent;
use boss_log_files::rotated_segments;

use crate::current_path;
use crate::timeline::{StageThresholds, StalledStage, TimelineState};

/// Identity of the file a cursor is anchored to, so a replaced or recreated
/// stream is detected rather than silently read at a meaningless offset.
///
/// On unix this is `(dev, ino)`, which changes on rotation, recreate, and
/// cross-filesystem moves. Elsewhere it degrades to a constant and the
/// length-shrink check below is the only truncation signal — acceptable
/// because Boss only ships on unix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId(u64, u64);

fn file_id(meta: &fs::Metadata) -> FileId {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileId(meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        FileId(0, 0)
    }
}

/// Where the next incremental read resumes from, and which file that offset
/// is meaningful for. `identity` is `None` when the index was last anchored
/// while the stream did not exist yet — then `offset` is 0 and reading from
/// the beginning is still a complete read, not a rebuild.
#[derive(Debug, Clone, Copy)]
struct Cursor {
    offset: u64,
    identity: Option<FileId>,
}

/// What one [`TimelineIndex::refresh`] actually did. Reported by the callers'
/// log lines so the per-pass cost is observable in production rather than
/// only under a profiler — `bytes_scanned` is the number this whole change
/// exists to keep flat as history grows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// Bytes of complete records consumed this pass.
    pub bytes_scanned: u64,
    /// Records folded into the index this pass.
    pub records_applied: usize,
    /// Lines that failed to parse and were skipped. The corpus contains at
    /// least one genuinely torn record; a skip is expected, not an error.
    pub malformed_lines: usize,
    /// True when the pass could not resume incrementally and re-read the
    /// whole stream (first pass, or the stream was rotated/truncated/removed).
    pub rebuilt: bool,
    /// Live (non-terminal) timelines held in memory after the pass.
    pub tracked_timelines: usize,
}

/// Result of folding one file (or one range of one file) into the index.
#[derive(Debug, Clone, Copy, Default)]
struct FoldOutcome {
    records: usize,
    malformed: usize,
    /// Bytes of *complete* lines consumed. A trailing partial line is left
    /// unconsumed so the next pass re-reads it once its newline lands.
    consumed: u64,
}

/// In-memory reduction of every live dispatch timeline under a state root,
/// maintained by tailing the flat dispatch-event stream.
///
/// Not `Sync`; use [`SharedTimelineIndex`] to hand it to more than one sweep.
#[derive(Debug)]
pub struct TimelineIndex {
    root: PathBuf,
    /// Live timelines only — see [`TimelineState::is_live`].
    states: HashMap<String, TimelineState>,
    cursor: Option<Cursor>,
    rebuilds: u64,
}

impl TimelineIndex {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            states: HashMap::new(),
            cursor: None,
            rebuilds: 0,
        }
    }

    /// Live timelines currently held.
    pub fn tracked_timelines(&self) -> usize {
        self.states.len()
    }

    /// How many times this index has had to re-read the whole stream. Steady
    /// state is 1 (the initial build); anything higher means the stream was
    /// rotated, truncated, or removed under us.
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// Fold everything appended since the previous pass. Falls back to a full
    /// re-read when there is no usable cursor — the first pass, or when the
    /// stream's identity changed (rotation/recreate) or its length dropped
    /// below the cursor (truncation).
    pub fn refresh(&mut self) -> Result<RefreshStats> {
        let path = current_path(&self.root);

        // Open first, then take identity from that same handle, so the
        // identity check and the bytes we read can never describe different
        // inodes.
        let opened = match fs::File::open(&path) {
            Ok(file) => {
                let meta = file
                    .metadata()
                    .with_context(|| format!("stat {} for incremental read", path.display()))?;
                Some((file, file_id(&meta), meta.len()))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
        };

        let Some((file, identity, len)) = opened else {
            return Ok(self.handle_missing_stream());
        };

        let resume_from = self.resume_offset(identity, len);
        let rebuilding = resume_from.is_none();
        let mut stats = RefreshStats {
            rebuilt: rebuilding,
            ..RefreshStats::default()
        };

        if rebuilding {
            self.states.clear();
            self.rebuilds += 1;
            // Rotated segments are immutable history. They do not exist today
            // (`current.jsonl` is never rotated) but a reader that spans them
            // is what any future rotation needs, and enumerating an empty set
            // costs one `read_dir`.
            for segment in rotated_segments(&path) {
                let outcome = fold_file(&segment, 0, &mut self.states)?;
                stats.absorb(outcome);
            }
        }

        let offset = resume_from.unwrap_or(0);
        let outcome = fold_open_file(file, &path, offset, &mut self.states)?;
        stats.absorb(outcome);

        self.cursor = Some(Cursor {
            offset: offset + outcome.consumed,
            identity: Some(identity),
        });
        stats.tracked_timelines = self.states.len();
        Ok(stats)
    }

    /// Fold an event the caller just emitted, so the detector's own
    /// `stage_stalled` output dedupes on the very next pass instead of
    /// waiting for it to come back around through the file tail. Folding it
    /// twice — once here, once when the tail reaches it — is a no-op.
    pub fn apply(&mut self, event: &DispatchEvent) {
        apply_event(&mut self.states, event);
    }

    /// Stalls the `stage_stalled` detector should emit, sorted by execution
    /// id so output is stable across passes.
    pub fn pending_stalls(&self, now_ms: u128, thresholds: &StageThresholds) -> Vec<StalledStage> {
        self.collect(|id, state| state.stall_to_emit(id, now_ms, thresholds))
    }

    /// Stalls past a flat, operator-facing threshold, sorted by execution id.
    /// Unlike [`Self::pending_stalls`] this does not dedupe against a prior
    /// `stage_stalled` record — see [`crate::persistently_stalled`].
    pub fn persistently_stalled(&self, now_ms: u128, threshold_ms: u128) -> Vec<StalledStage> {
        self.collect(|id, state| state.persistent_stall(id, now_ms, threshold_ms))
    }

    fn collect(&self, pick: impl Fn(&str, &TimelineState) -> Option<StalledStage>) -> Vec<StalledStage> {
        let mut out: Vec<StalledStage> = self.states.iter().filter_map(|(id, state)| pick(id, state)).collect();
        out.sort_by(|a, b| a.execution_id.cmp(&b.execution_id));
        out
    }

    /// The offset to resume from, or `None` when the pass must rebuild.
    fn resume_offset(&self, identity: FileId, len: u64) -> Option<u64> {
        let cursor = self.cursor?;
        match cursor.identity {
            // Anchored on a file that has since been replaced, or truncated
            // below where we stopped: records we never saw may be gone, so
            // in-memory state can no longer be trusted.
            Some(anchored) if anchored != identity || len < cursor.offset => None,
            // Anchored while the stream did not exist yet — offset is 0, so
            // reading from the start is a complete read, not a rebuild.
            Some(_) | None => Some(cursor.offset),
        }
    }

    /// The stream is not on disk. Either it never was (nothing has been
    /// dispatched yet) or it was removed under us, in which case anything we
    /// hold may be stale and is dropped.
    fn handle_missing_stream(&mut self) -> RefreshStats {
        let anchored_on_a_real_file = self.cursor.is_some_and(|c| c.identity.is_some());
        if anchored_on_a_real_file {
            self.states.clear();
            self.rebuilds += 1;
        }
        self.cursor = Some(Cursor {
            offset: 0,
            identity: None,
        });
        RefreshStats {
            rebuilt: anchored_on_a_real_file,
            tracked_timelines: self.states.len(),
            ..RefreshStats::default()
        }
    }
}

impl RefreshStats {
    fn absorb(&mut self, outcome: FoldOutcome) {
        self.bytes_scanned += outcome.consumed;
        self.records_applied += outcome.records;
        self.malformed_lines += outcome.malformed;
    }
}

/// Fold `path` from `from` onward. A missing file contributes nothing.
fn fold_file(path: &Path, from: u64, states: &mut HashMap<String, TimelineState>) -> Result<FoldOutcome> {
    match fs::File::open(path) {
        Ok(file) => fold_open_file(file, path, from, states),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(FoldOutcome::default()),
        Err(err) => Err(err).with_context(|| format!("opening {}", path.display())),
    }
}

/// Stream `file` from byte `from` to EOF, folding each complete record into
/// `states`.
///
/// Streaming (rather than reading the range into a `Vec<String>` the way
/// `boss_log_files::read_new_content` does) is what makes the same code
/// usable for both the incremental tail — a few KB — and the initial build,
/// which reads the entire ~103 MB stream and must not materialize it.
fn fold_open_file(
    file: fs::File,
    path: &Path,
    from: u64,
    states: &mut HashMap<String, TimelineState>,
) -> Result<FoldOutcome> {
    let mut file = file;
    if from > 0 {
        file.seek(SeekFrom::Start(from))
            .with_context(|| format!("seeking {} to offset {from}", path.display()))?;
    }
    let mut reader = BufReader::new(file);
    let mut outcome = FoldOutcome::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            // The sink appends the record and its newline as two separate
            // writes, so a reader can observe the gap. Leave these bytes
            // unconsumed: the next pass re-reads the line once it is whole,
            // instead of permanently dropping a record we happened to catch
            // mid-write.
            break;
        }
        outcome.consumed += read as u64;
        let trimmed = trim_record(&line);
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_slice::<DispatchEvent>(trimmed) {
            Ok(event) => {
                apply_event(states, &event);
                outcome.records += 1;
            }
            // A genuinely unparseable record (the corpus contains one) must
            // not abort the pass — it is already consumed, so skipping it
            // here matches what a full rescan does.
            Err(_) => outcome.malformed += 1,
        }
    }
    Ok(outcome)
}

fn trim_record(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r' || line[end - 1] == b' ') {
        end -= 1;
    }
    let mut start = 0;
    while start < end && (line[start] == b' ' || line[start] == b'\t') {
        start += 1;
    }
    &line[start..end]
}

/// Fold one event into `states`, keeping only live timelines resident.
///
/// Dropping a timeline the moment it goes terminal is what bounds memory to
/// in-flight dispatches rather than all 16k+ historical executions. It is
/// safe because [`TimelineState`] is suffix-determined: if a dropped
/// execution ever emits another record, folding that record alone reproduces
/// exactly the state a full rescan would compute.
fn apply_event(states: &mut HashMap<String, TimelineState>, event: &DispatchEvent) {
    if let Some(state) = states.get_mut(&event.execution_id) {
        state.apply(event);
        if !state.is_live() {
            states.remove(&event.execution_id);
        }
        return;
    }
    let mut state = TimelineState::default();
    state.apply(event);
    if state.is_live() {
        states.insert(event.execution_id.clone(), state);
    }
}

/// Clonable handle to one [`TimelineIndex`] shared by the stage-stalled
/// detector (15s) and the persistent-stall escalation sweep (60s).
///
/// One index rather than one each: both sweeps need exactly the same derived
/// state, so sharing means the stream is tailed once per pass instead of
/// twice and the state is held once instead of twice. `refresh` is
/// idempotent, so whichever sweep runs first advances the cursor and the
/// other simply reads a stream that is already up to date.
#[derive(Debug, Clone)]
pub struct SharedTimelineIndex(Arc<Mutex<TimelineIndex>>);

impl SharedTimelineIndex {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self(Arc::new(Mutex::new(TimelineIndex::new(root))))
    }

    /// Refresh, then report the stalls the `stage_stalled` detector should
    /// emit. Refresh and query happen under one lock so the two sweeps can
    /// never interleave against a half-updated index.
    pub fn refresh_and_pending_stalls(
        &self,
        now_ms: u128,
        thresholds: &StageThresholds,
    ) -> Result<(RefreshStats, Vec<StalledStage>)> {
        self.with(|index| {
            let stats = index.refresh()?;
            Ok((stats, index.pending_stalls(now_ms, thresholds)))
        })
    }

    /// Refresh, then report stalls past a flat operator-facing threshold.
    pub fn refresh_and_persistently_stalled(
        &self,
        now_ms: u128,
        threshold_ms: u128,
    ) -> Result<(RefreshStats, Vec<StalledStage>)> {
        self.with(|index| {
            let stats = index.refresh()?;
            Ok((stats, index.persistently_stalled(now_ms, threshold_ms)))
        })
    }

    /// See [`TimelineIndex::apply`].
    pub fn apply(&self, event: &DispatchEvent) {
        self.with(|index| index.apply(event));
    }

    pub fn tracked_timelines(&self) -> usize {
        self.with(|index| index.tracked_timelines())
    }

    pub fn rebuilds(&self) -> u64 {
        self.with(|index| index.rebuilds())
    }

    /// A poisoned lock means some other sweep panicked mid-refresh. The
    /// index is still structurally sound — the fold is idempotent and the
    /// cursor only advances after a successful read — so recovering the
    /// guard and carrying on is strictly better than propagating the panic
    /// into every subsequent sweep.
    fn with<R>(&self, f: impl FnOnce(&mut TimelineIndex) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    use boss_dispatch_events::{DispatchEventSink, JsonlFileSink, Outcome, Stage};
    use tempfile::TempDir;

    fn flat(secs: u64) -> StageThresholds {
        StageThresholds::new(Duration::from_secs(secs))
    }

    async fn emit(root: &Path, stage: Stage, outcome: Outcome, execution_id: &str, ts: u128) {
        let sink = JsonlFileSink::new(root);
        let mut event = DispatchEvent::new(stage, outcome, execution_id);
        event.ts_epoch_ms = ts;
        sink.emit(event).await;
    }

    fn stream(root: &Path) -> PathBuf {
        current_path(root)
    }

    fn append_raw(root: &Path, bytes: &[u8]) {
        let path = stream(root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
        file.write_all(bytes).unwrap();
    }

    #[tokio::test]
    async fn first_refresh_builds_from_the_whole_stream_then_tails() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::RequestRecorded, Outcome::Ok, "exec-1", 0).await;
        emit(dir.path(), Stage::WorkerClaimed, Outcome::Ok, "exec-1", 1_000).await;

        let mut index = TimelineIndex::new(dir.path());
        let first = index.refresh().unwrap();
        assert!(first.rebuilt);
        assert_eq!(first.records_applied, 2);
        assert_eq!(first.tracked_timelines, 1);
        assert!(first.bytes_scanned > 0);

        // Nothing appended: a pass must read zero bytes and stay put.
        let idle = index.refresh().unwrap();
        assert!(!idle.rebuilt);
        assert_eq!(idle.bytes_scanned, 0);
        assert_eq!(idle.records_applied, 0);

        // One new record: only that record is read, not the history.
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-1", 2_000).await;
        let tail = index.refresh().unwrap();
        assert!(!tail.rebuilt);
        assert_eq!(tail.records_applied, 1);
        assert!(
            tail.bytes_scanned < first.bytes_scanned,
            "incremental pass must read less than the full stream"
        );
        assert_eq!(index.rebuilds(), 1, "steady state must not re-read the stream");
    }

    /// The property the whole change exists for: per-pass cost tracks bytes
    /// appended, not bytes on disk.
    #[tokio::test]
    async fn steady_state_cost_is_independent_of_history_size() {
        let dir = TempDir::new().unwrap();
        for i in 0..500u128 {
            emit(dir.path(), Stage::PaneSpawned, Outcome::Ok, &format!("exec-old-{i}"), i).await;
        }
        let mut index = TimelineIndex::new(dir.path());
        let build = index.refresh().unwrap();
        assert!(build.rebuilt);
        assert_eq!(build.records_applied, 500);
        // Every one of those is terminal, so none is retained.
        assert_eq!(build.tracked_timelines, 0);

        emit(dir.path(), Stage::RequestRecorded, Outcome::Ok, "exec-new", 9_000).await;
        let tail = index.refresh().unwrap();
        assert_eq!(tail.records_applied, 1);
        assert!(
            tail.bytes_scanned * 50 < build.bytes_scanned,
            "one appended record cost {} bytes against a {}-byte history",
            tail.bytes_scanned,
            build.bytes_scanned
        );
    }

    #[tokio::test]
    async fn stalls_are_detected_from_the_incremental_view() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-stuck", 1_000).await;
        emit(dir.path(), Stage::PaneSpawned, Outcome::Ok, "exec-done", 1_000).await;

        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();

        assert!(index.pending_stalls(5_000, &flat(5)).is_empty(), "not yet past 5s");
        let stalls = index.pending_stalls(10_000, &flat(5));
        assert_eq!(stalls.len(), 1);
        assert_eq!(stalls[0].execution_id, "exec-stuck");
        assert_eq!(stalls[0].stalled_stage, "cube_change_created");
        assert_eq!(stalls[0].elapsed_in_stage_ms, 9_000);
    }

    #[tokio::test]
    async fn a_stalled_timeline_that_progresses_stops_being_reported() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-1", 1_000).await;
        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();
        assert_eq!(index.pending_stalls(10_000, &flat(5)).len(), 1);

        emit(dir.path(), Stage::PaneSpawned, Outcome::Ok, "exec-1", 9_500).await;
        index.refresh().unwrap();
        assert!(index.pending_stalls(10_000, &flat(5)).is_empty());
        assert_eq!(index.tracked_timelines(), 0, "terminal timelines must be dropped");
    }

    /// The detector's own output has to suppress a repeat on the next pass,
    /// whether it arrives via `apply` or via the file tail.
    #[tokio::test]
    async fn emitted_stage_stalled_events_dedupe_the_next_pass() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-1", 1_000).await;
        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();

        let stalls = index.pending_stalls(10_000, &flat(5));
        assert_eq!(stalls.len(), 1);
        let event = crate::build_stalled_event(&stalls[0]);
        index.apply(&event);
        assert!(index.pending_stalls(11_000, &flat(5)).is_empty());

        // And once the same record reaches the stream, re-folding it changes
        // nothing.
        JsonlFileSink::new(dir.path()).emit(event).await;
        index.refresh().unwrap();
        assert!(index.pending_stalls(12_000, &flat(5)).is_empty());
    }

    #[tokio::test]
    async fn persistently_stalled_ignores_a_prior_flag() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-1", 0).await;
        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();
        let event = crate::build_stalled_event(&index.pending_stalls(10_000, &flat(5))[0]);
        index.apply(&event);

        assert!(index.pending_stalls(400_000, &flat(5)).is_empty());
        let persistent = index.persistently_stalled(400_000, 300_000);
        assert_eq!(persistent.len(), 1);
        assert_eq!(persistent[0].elapsed_in_stage_ms, 400_000);
    }

    /// A record caught between its payload write and its newline write must
    /// be re-read whole on a later pass, never consumed half-formed.
    #[tokio::test]
    async fn a_torn_trailing_record_is_re_read_once_complete() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::RequestRecorded, Outcome::Ok, "exec-1", 0).await;
        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();

        let torn = serde_json::to_vec(&{
            let mut event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-1");
            event.ts_epoch_ms = 1_000;
            event
        })
        .unwrap();
        append_raw(dir.path(), &torn);

        let partial = index.refresh().unwrap();
        assert_eq!(partial.records_applied, 0, "a newline-less tail is not a record yet");
        assert_eq!(partial.bytes_scanned, 0);

        append_raw(dir.path(), b"\n");
        let complete = index.refresh().unwrap();
        assert_eq!(complete.records_applied, 1);
        assert_eq!(
            index.pending_stalls(10_000, &flat(5))[0].stalled_stage,
            "cube_change_created"
        );
    }

    /// An unparseable line is skipped, not fatal — the production corpus has
    /// one — and the pass still consumes it so it is not retried forever.
    #[tokio::test]
    async fn an_unparseable_line_is_skipped_without_aborting_the_pass() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::RequestRecorded, Outcome::Ok, "exec-1", 0).await;
        append_raw(dir.path(), b"{not json at all\n");
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-2", 1_000).await;

        let mut index = TimelineIndex::new(dir.path());
        let stats = index.refresh().unwrap();
        assert_eq!(stats.malformed_lines, 1);
        assert_eq!(stats.records_applied, 2);
        assert_eq!(index.tracked_timelines(), 2);

        let idle = index.refresh().unwrap();
        assert_eq!(idle.bytes_scanned, 0, "the bad line must not be re-read forever");
    }

    #[tokio::test]
    async fn truncation_forces_a_rebuild() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-1", 1_000).await;
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-2", 1_000).await;
        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();
        assert_eq!(index.tracked_timelines(), 2);

        // Truncate in place: same inode, shorter than the cursor.
        fs::File::create(stream(dir.path())).unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-3", 1_000).await;

        let stats = index.refresh().unwrap();
        assert!(stats.rebuilt);
        assert_eq!(index.tracked_timelines(), 1, "only the surviving record counts");
        assert_eq!(index.pending_stalls(10_000, &flat(5))[0].execution_id, "exec-3");
    }

    #[tokio::test]
    async fn replacing_the_stream_file_forces_a_rebuild() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-1", 1_000).await;
        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();

        // Rename the live file aside — a new inode takes its place, exactly
        // what a rotation would do.
        let live = stream(dir.path());
        let rotated = boss_log_files::rotated_segment_path(&live, 1_700_000_000);
        fs::rename(&live, &rotated).unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-2", 1_000).await;

        let stats = index.refresh().unwrap();
        assert!(stats.rebuilt, "a replaced inode must not be tailed at a stale offset");
        // The rebuild spans the rotated segment as well as the live file.
        let ids: Vec<String> = index
            .pending_stalls(10_000, &flat(5))
            .into_iter()
            .map(|s| s.execution_id)
            .collect();
        assert_eq!(ids, vec!["exec-1".to_owned(), "exec-2".to_owned()]);
    }

    #[test]
    fn an_absent_stream_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let mut index = TimelineIndex::new(dir.path());
        let stats = index.refresh().unwrap();
        assert!(!stats.rebuilt);
        assert_eq!(stats.tracked_timelines, 0);
        assert!(index.pending_stalls(9_999_999, &flat(5)).is_empty());
    }

    /// A state root that gains its first event after the index started
    /// polling an empty one must be picked up incrementally, not rebuilt.
    #[tokio::test]
    async fn a_stream_that_appears_later_is_tailed_from_zero() {
        let dir = TempDir::new().unwrap();
        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();

        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-1", 1_000).await;
        let stats = index.refresh().unwrap();
        assert!(!stats.rebuilt);
        assert_eq!(stats.records_applied, 1);
        assert_eq!(index.rebuilds(), 0);
    }

    #[tokio::test]
    async fn a_removed_stream_drops_state_and_rebuilds() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-1", 1_000).await;
        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();
        assert_eq!(index.tracked_timelines(), 1);

        fs::remove_file(stream(dir.path())).unwrap();
        let stats = index.refresh().unwrap();
        assert!(stats.rebuilt);
        assert_eq!(index.tracked_timelines(), 0);
    }

    /// The wedged-engine guarantee: the pure file-scan sweeps still work and
    /// still agree with the index, so nothing that depends on them has been
    /// quietly re-pointed at engine-held memory.
    #[tokio::test]
    async fn index_agrees_with_the_file_scan_sweeps() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::RequestRecorded, Outcome::Ok, "exec-a", 0).await;
        emit(dir.path(), Stage::WorkerClaimed, Outcome::Ok, "exec-a", 1_000).await;
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-b", 500).await;
        emit(dir.path(), Stage::PaneSpawned, Outcome::Ok, "exec-c", 500).await;
        emit(dir.path(), Stage::RunStarted, Outcome::Error, "exec-d", 500).await;
        emit(dir.path(), Stage::CubeWorkspaceLeased, Outcome::Ok, "exec-e", 700).await;

        let mut index = TimelineIndex::new(dir.path());
        index.refresh().unwrap();

        let thresholds = flat(5);
        let mut scanned = crate::pending_stalls(dir.path(), 10_000, &thresholds).unwrap();
        scanned.sort_by(|a, b| a.execution_id.cmp(&b.execution_id));
        assert_eq!(index.pending_stalls(10_000, &thresholds), scanned);
        assert_eq!(scanned.len(), 3, "exec-a, exec-b, exec-e");

        // Emit the flags both ways and re-check: dedupe must agree too.
        for stall in &scanned {
            let event = crate::build_stalled_event(stall);
            JsonlFileSink::new(dir.path()).emit(event).await;
        }
        index.refresh().unwrap();
        let mut scanned = crate::pending_stalls(dir.path(), 11_000, &thresholds).unwrap();
        scanned.sort_by(|a, b| a.execution_id.cmp(&b.execution_id));
        assert!(scanned.is_empty(), "every stall is now flagged");
        assert_eq!(index.pending_stalls(11_000, &thresholds), scanned);

        let mut scanned = crate::persistently_stalled(dir.path(), 400_000, 300_000).unwrap();
        scanned.sort_by(|a, b| a.execution_id.cmp(&b.execution_id));
        assert_eq!(index.persistently_stalled(400_000, 300_000), scanned);
        assert_eq!(scanned.len(), 3, "the flat sweep ignores flags");
    }

    #[test]
    fn trim_record_strips_line_endings_and_padding() {
        assert_eq!(trim_record(b"  {}\r\n"), b"{}");
        assert_eq!(trim_record(b"\n"), b"");
        assert_eq!(trim_record(b"{}"), b"{}");
    }

    #[tokio::test]
    async fn shared_index_serves_both_sweeps_from_one_tail() {
        let dir = TempDir::new().unwrap();
        emit(dir.path(), Stage::CubeChangeCreated, Outcome::Ok, "exec-1", 0).await;
        let shared = SharedTimelineIndex::new(dir.path());

        let (first, stalls) = shared.refresh_and_pending_stalls(10_000, &flat(5)).unwrap();
        assert!(first.rebuilt);
        assert_eq!(stalls.len(), 1);

        // The second sweep reuses the tail the first one already advanced.
        let (second, persistent) = shared.refresh_and_persistently_stalled(400_000, 300_000).unwrap();
        assert!(!second.rebuilt);
        assert_eq!(second.bytes_scanned, 0);
        assert_eq!(persistent.len(), 1);
        assert_eq!(shared.rebuilds(), 1);
        assert_eq!(shared.tracked_timelines(), 1);
    }
}
