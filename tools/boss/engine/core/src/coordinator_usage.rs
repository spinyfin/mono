//! Coordinator session token usage capture behind a pluggable source interface.
//!
//! The Boss coordinator currently runs as a long-lived Claude Code CLI session
//! in a dedicated tmux pane (`boss-session`). [`ClaudeTranscriptCoordinatorUsageSource`]
//! reads token usage directly from that session's Claude Code JSONL transcripts
//! and sibling `<session_id>/subagents/*.jsonl` files — never estimating from
//! text length — deduplicates records by `message.id` and `turn_duration` by
//! `uuid`, buckets usage by `(session_id, UTC hour, model)`, and persists
//! idempotent snapshots into `WorkDb::upsert_coordinator_session_usage`.
//!
//! Transcripts are read incrementally with the same [`TranscriptTail`] the run
//! cost capture uses, in its lenient mode: a malformed line is skipped and
//! counted instead of abandoning the file. The Claude message decoding and the
//! subagent discovery are shared with [`crate::run_cost`]; only the hourly
//! bucketing and the [`CoordinatorUsageSource`] seam live here.
//!
//! An incomplete read (a skipped line, an unreadable file) never discards
//! usage that was already measured: the snapshot carries a `capture_gap`, which
//! the store merges into the stored slices and records as a coverage gap.
//!
//! A future ACP coordinator backend that receives `usage_update` messages can
//! implement [`CoordinatorUsageSource`] or push [`CoordinatorSessionSnapshot`]s
//! directly through [`CoordinatorUsageRecorder::record_snapshot`] without
//! changing storage, `boss cost`, or the Token Usage window.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use boss_protocol::DRIVER_SLUG_CLAUDE;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::coordinator_handoff::transcript_projects_dir_in_config;
use crate::run_cost::{
    ClaudeRecord, MessageUsage, decode_claude_record, enumerate_subagent_transcripts, merge_claude_chunk,
};
use crate::transcript_tail::TranscriptTail;
use crate::work::{
    COORDINATOR_UNREADABLE_SPAWN_PREFIX, CoordinatorSessionSnapshot, CoordinatorTmuxRecord, CoordinatorUsageSlice,
    WorkDb,
};

/// Source identifier persisted on slices captured from Claude Code transcripts.
pub const CLAUDE_TRANSCRIPT_SOURCE: &str = "claude_transcript";

const HOUR_SECS: i64 = 3_600;

/// Inputs supplied to a [`CoordinatorUsageSource`] on each sync pass.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct CoordinatorUsageContext {
    /// Coordinator session working directory (`boss-session` in production, or
    /// the fixture-scoped session directory in an isolated engine).
    pub working_directory: PathBuf,
    /// Optional explicit Claude config directory (`~/.claude` / `CLAUDE_CONFIG_DIR`).
    /// When `None`, resolved from `CLAUDE_CONFIG_DIR` then `$HOME/.claude`.
    pub claude_config_dir: Option<PathBuf>,
    /// Current durable coordinator tmux record from `WorkDb`, if one exists.
    pub live_record: Option<CoordinatorTmuxRecord>,
    /// Current wall-clock time (epoch seconds).
    pub now_epoch_s: i64,
}

impl CoordinatorUsageContext {
    fn resolved_claude_config_dir(&self) -> Option<PathBuf> {
        self.claude_config_dir.clone().or_else(|| {
            std::env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        })
    }
}

/// Output of one [`CoordinatorUsageSource::collect`] pass.
#[derive(Debug, Default, Clone)]
pub struct CoordinatorUsageCollection {
    /// Sessions whose slices changed (or were newly discovered) and should be
    /// persisted to `WorkDb`.
    pub snapshots: Vec<CoordinatorSessionSnapshot>,
    /// Session ids that are currently active. Any previously `active` session
    /// in `WorkDb` outside this set is marked `completed` while keeping all of
    /// its stored token counts.
    pub active_session_ids: Vec<String>,
    /// Spawn tokens already backed by a real captured session (even when
    /// unchanged since the last poll), so any prior unmeasured `spawn:<token>`
    /// placeholder can be retired.
    pub covered_spawn_tokens: Vec<String>,
    /// Session ids currently present in the projects directory when the
    /// directory listing succeeded. `None` when the source does not list a
    /// directory or the listing failed, so stale-fact pruning is skipped.
    pub listed_session_ids: Option<Vec<String>>,
}

/// Small source interface for coordinator token usage.
///
/// Today [`ClaudeTranscriptCoordinatorUsageSource`] reads Claude Code CLI
/// transcripts; the native coordinator chat ACP backend will feed the same
/// [`CoordinatorSessionSnapshot`] / [`CoordinatorUsageSlice`] shape later.
#[async_trait::async_trait]
pub trait CoordinatorUsageSource: Send + Sync {
    fn source_name(&self) -> &'static str;

    async fn collect(&self, context: &CoordinatorUsageContext) -> CoordinatorUsageCollection;

    /// Called with the session ids whose snapshots from the latest `collect`
    /// were durably persisted. A source that skips unchanged sessions must
    /// only treat a snapshot as delivered once it is acknowledged here, so a
    /// failed write is retried on the next pass instead of being stranded.
    async fn acknowledge(&self, _persisted_session_ids: &[String]) {}
}

#[derive(Default)]
struct RecorderBookkeeping {
    last_active_sessions: Option<Vec<String>>,
    cleared_spawn_tokens: HashSet<String>,
    last_pruned_listed_sessions: Option<Vec<String>>,
}

/// Coordinates polling a [`CoordinatorUsageSource`] (or accepting pushed
/// snapshots) and persisting them into [`WorkDb`] without overlapping passes.
pub struct CoordinatorUsageRecorder {
    source: Arc<dyn CoordinatorUsageSource>,
    sync_lock: Mutex<()>,
    last_sync: StdMutex<Option<Instant>>,
    bookkeeping: StdMutex<RecorderBookkeeping>,
}

impl CoordinatorUsageRecorder {
    pub fn new(source: Arc<dyn CoordinatorUsageSource>) -> Self {
        Self {
            source,
            sync_lock: Mutex::new(()),
            last_sync: StdMutex::new(None),
            bookkeeping: StdMutex::new(RecorderBookkeeping::default()),
        }
    }

    pub fn for_claude_transcripts() -> Self {
        Self::new(Arc::new(ClaudeTranscriptCoordinatorUsageSource::new()))
    }

    pub fn source_name(&self) -> &'static str {
        self.source.source_name()
    }

    /// Run one full incremental sync from the configured source into `work_db`,
    /// waiting for any pass already in flight. Returns the number of slices
    /// persisted.
    pub async fn sync(&self, work_db: &WorkDb, context: &CoordinatorUsageContext) -> Result<usize> {
        let _guard = self.sync_lock.lock().await;
        self.sync_locked(work_db, context).await
    }

    async fn sync_locked(&self, work_db: &WorkDb, context: &CoordinatorUsageContext) -> Result<usize> {
        let collection = self.source.collect(context).await;

        let mut active_sorted = collection.active_session_ids.clone();
        active_sorted.sort();
        active_sorted.dedup();
        let listed_sorted = collection.listed_session_ids.clone().map(|mut ids| {
            ids.sort();
            ids.dedup();
            ids
        });

        let (tokens_to_clear, run_mark_inactive, run_prune) = {
            let guard = self.bookkeeping.lock().expect("recorder bookkeeping poisoned");
            let tokens: Vec<String> = collection
                .covered_spawn_tokens
                .iter()
                .filter(|t| !guard.cleared_spawn_tokens.contains(*t))
                .cloned()
                .collect();
            let mark_inactive = guard.last_active_sessions.as_ref() != Some(&active_sorted);
            let prune = listed_sorted.is_some() && guard.last_pruned_listed_sessions != listed_sorted;
            (tokens, mark_inactive, prune)
        };

        let db = work_db.clone();
        let snapshots = collection.snapshots;
        let active_for_db = active_sorted.clone();
        let listed_for_db = listed_sorted.clone();
        let tokens_for_db = tokens_to_clear.clone();
        let outcome = crate::work::db_diagnostics::spawn_blocking(move || {
            let mut slices_written = 0usize;
            let mut persisted = Vec::with_capacity(snapshots.len());
            let mut first_error = None;
            // One failing write must not strand the snapshots after it: keep going,
            // and acknowledge only what actually committed.
            for snapshot in &snapshots {
                match db.upsert_coordinator_session_usage(snapshot) {
                    Ok(written) => {
                        slices_written += written;
                        persisted.push(snapshot.session_id.clone());
                    }
                    Err(error) => {
                        tracing::warn!(
                            session_id = %snapshot.session_id,
                            error = %format!("{error:#}"),
                            "coordinator usage snapshot not persisted; will retry on the next sync"
                        );
                        first_error.get_or_insert(error);
                    }
                }
            }
            if let Some(error) = first_error {
                return (persisted, Err(error));
            }
            for token in &tokens_for_db {
                if let Err(error) = db.clear_coordinator_spawn_placeholder(token) {
                    return (persisted, Err(error));
                }
            }
            if run_mark_inactive {
                let active_refs: Vec<&str> = active_for_db.iter().map(String::as_str).collect();
                if let Err(error) = db.mark_inactive_coordinator_sessions_completed(&active_refs) {
                    return (persisted, Err(error));
                }
            }
            if run_prune && let Some(listed) = listed_for_db.as_ref() {
                let listed_refs: Vec<&str> = listed.iter().map(String::as_str).collect();
                if let Err(error) = db.prune_stale_coordinator_usage_facts(&listed_refs) {
                    return (persisted, Err(error));
                }
            }
            (persisted, Ok(slices_written))
        })
        .await
        .map_err(|err| anyhow::anyhow!("coordinator usage sync task failed: {err}"))?;

        let (persisted, result) = outcome;
        self.source.acknowledge(&persisted).await;
        let slices_written = result?;
        if let Ok(mut guard) = self.bookkeeping.lock() {
            for token in tokens_to_clear {
                guard.cleared_spawn_tokens.insert(token);
            }
            if run_mark_inactive {
                guard.last_active_sessions = Some(active_sorted);
            }
            if run_prune {
                guard.last_pruned_listed_sessions = listed_sorted;
            }
        }
        if let Ok(mut last) = self.last_sync.lock() {
            *last = Some(Instant::now());
        }
        Ok(slices_written)
    }

    /// Sync only if at least `min_interval` has elapsed since the previous
    /// completed sync, and skip (rather than queue behind) a pass that is
    /// already running. Used by background refreshes so repeated requests stay
    /// cheap while always reflecting recent turns.
    pub async fn sync_if_stale(
        &self,
        work_db: &WorkDb,
        context: &CoordinatorUsageContext,
        min_interval: Duration,
    ) -> Result<usize> {
        let fresh = self
            .last_sync
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .is_some_and(|instant| instant.elapsed() < min_interval);
        if fresh {
            return Ok(0);
        }
        let Ok(_guard) = self.sync_lock.try_lock() else {
            return Ok(0);
        };
        self.sync_locked(work_db, context).await
    }

    /// Direct push entrypoint for event-driven sources (e.g. a future ACP
    /// `usage_update` handler) that already hold a [`CoordinatorSessionSnapshot`].
    pub fn record_snapshot(&self, work_db: &WorkDb, snapshot: &CoordinatorSessionSnapshot) -> Result<usize> {
        work_db.upsert_coordinator_session_usage(snapshot)
    }
}

/// What a session's last emitted snapshot was built from. A session is
/// re-emitted whenever any part changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EmitKey {
    progress: u64,
    status: String,
    spawn_token: Option<String>,
    launched_model: Option<String>,
    gap: Option<String>,
}

/// One transcript file read incrementally into a session accumulator.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct TrackedTail {
    path: PathBuf,
    tail: TranscriptTail,
    generation: u64,
    next_value_index: usize,
    last_model: Option<String>,
    last_assistant_ts: Option<i64>,
    last_assistant_id: Option<String>,
    skipped_lines: usize,
    first_skip_error: Option<String>,
    read_error: Option<String>,
}

impl TrackedTail {
    fn new(path: &Path) -> Self {
        Self::builder()
            .path(path.to_owned())
            .tail(TranscriptTail::new(path))
            .generation(0)
            .next_value_index(0)
            .skipped_lines(0)
            .build()
    }
}

/// Everything known about one coordinator session, kept across polls so each
/// pass reads only the bytes appended since the previous one.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct SessionCapture {
    parent: TrackedTail,
    subagents: BTreeMap<PathBuf, TrackedTail>,
    acc: SessionTranscriptAccumulator,
    dirty_messages: HashSet<String>,
    dirty_durations: HashSet<String>,
    progress: u64,
    acknowledged: Option<EmitKey>,
}

/// Outcome of polling one session.
enum PollOutcome {
    Applied,
    /// A transcript was truncated or replaced: everything folded so far
    /// belongs to an obsolete generation and the session must be re-read.
    Rebuild,
}

impl SessionCapture {
    fn new(parent_path: &Path) -> Self {
        Self {
            parent: TrackedTail::new(parent_path),
            subagents: BTreeMap::new(),
            acc: SessionTranscriptAccumulator::default(),
            dirty_messages: HashSet::new(),
            dirty_durations: HashSet::new(),
            progress: 0,
            acknowledged: None,
        }
    }

    async fn poll(&mut self, subagent_paths: &[PathBuf], fallback_ts: i64) -> PollOutcome {
        // Subagents first, parent last, so a shared message.id keeps the
        // parent's record if timestamps tie.
        for path in subagent_paths {
            self.subagents
                .entry(path.clone())
                .or_insert_with(|| TrackedTail::new(path));
        }
        let paths: Vec<PathBuf> = self.subagents.keys().cloned().collect();
        for path in paths {
            if let Some(tracked) = self.subagents.get_mut(&path)
                && matches!(
                    poll_tracked(
                        tracked,
                        &mut self.acc,
                        &mut self.dirty_messages,
                        &mut self.dirty_durations,
                        &mut self.progress,
                        fallback_ts,
                    )
                    .await,
                    PollOutcome::Rebuild
                )
            {
                return PollOutcome::Rebuild;
            }
        }
        poll_tracked(
            &mut self.parent,
            &mut self.acc,
            &mut self.dirty_messages,
            &mut self.dirty_durations,
            &mut self.progress,
            fallback_ts,
        )
        .await
    }

    fn skipped_lines(&self) -> usize {
        self.parent.skipped_lines + self.subagents.values().map(|t| t.skipped_lines).sum::<usize>()
    }

    /// Why this session's capture is incomplete, or `None` when every file was
    /// read in full.
    fn gap_reason(&self, subagent_dir_error: Option<&str>) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(error) = subagent_dir_error {
            parts.push(format!("Coordinator {}", lower_first(error)));
        }
        if let Some(error) = &self.parent.read_error {
            parts.push(format!(
                "Coordinator transcript unreadable ({}): {error}",
                self.parent.path.display()
            ));
        }
        for tracked in self.subagents.values() {
            if let Some(error) = &tracked.read_error {
                parts.push(format!(
                    "Coordinator subagent transcript unreadable ({}): {error}",
                    tracked.path.display()
                ));
            }
        }
        let skipped = self.skipped_lines();
        if skipped > 0 {
            let first = std::iter::once(&self.parent)
                .chain(self.subagents.values())
                .find_map(|tracked| tracked.first_skip_error.as_deref())
                .unwrap_or("unknown error");
            parts.push(format!(
                "{skipped} malformed transcript line(s) skipped (first: {first})"
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

async fn poll_tracked(
    tracked: &mut TrackedTail,
    acc: &mut SessionTranscriptAccumulator,
    dirty_messages: &mut HashSet<String>,
    dirty_durations: &mut HashSet<String>,
    progress: &mut u64,
    fallback_ts: i64,
) -> PollOutcome {
    if !tracked.path.exists() {
        tracked.read_error = Some("previously observed transcript is missing".into());
        return PollOutcome::Applied;
    }
    match tracked.tail.poll_lenient().await {
        Ok(poll) => {
            tracked.read_error = None;
            if tracked.tail.generation() != tracked.generation {
                return PollOutcome::Rebuild;
            }
            tracked.skipped_lines += poll.skipped_lines;
            if tracked.first_skip_error.is_none() {
                tracked.first_skip_error = poll.first_skip_error;
            }
            *progress += poll.skipped_lines as u64;
            let file_key = tracked.path.to_string_lossy().into_owned();
            for value in &poll.values {
                if let Some(dirty) = acc.ingest_value(&file_key, tracked.next_value_index, value, fallback_ts, tracked)
                {
                    match dirty {
                        DirtyFact::Message(id) => {
                            dirty_messages.insert(id);
                        }
                        DirtyFact::Duration(id) => {
                            dirty_durations.insert(id);
                        }
                    }
                    *progress += 1;
                }
                tracked.next_value_index += 1;
            }
        }
        Err(error) => {
            tracked.read_error = Some(error.to_string());
        }
    }
    PollOutcome::Applied
}

#[derive(Default)]
struct SourceState {
    sessions: HashMap<String, SessionCapture>,
    /// Keys of the snapshots handed out by the latest `collect`, moved to the
    /// session's `acknowledged` key once the write commits.
    pending: HashMap<String, EmitKey>,
}

/// Reads coordinator session usage from Claude Code's project transcript
/// directory (`~/.claude/projects/<encoded-cwd>/*.jsonl` plus sibling
/// `<session_id>/subagents/*.jsonl`).
#[derive(Default)]
pub struct ClaudeTranscriptCoordinatorUsageSource {
    state: Mutex<SourceState>,
}

impl ClaudeTranscriptCoordinatorUsageSource {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl CoordinatorUsageSource for ClaudeTranscriptCoordinatorUsageSource {
    fn source_name(&self) -> &'static str {
        CLAUDE_TRANSCRIPT_SOURCE
    }

    async fn acknowledge(&self, persisted_session_ids: &[String]) {
        let mut state = self.state.lock().await;
        for session_id in persisted_session_ids {
            if let Some(key) = state.pending.remove(session_id)
                && let Some(capture) = state.sessions.get_mut(session_id)
            {
                capture.acknowledged = Some(key);
                capture.dirty_messages.clear();
                capture.dirty_durations.clear();
            }
        }
        state.pending.clear();
    }

    async fn collect(&self, context: &CoordinatorUsageContext) -> CoordinatorUsageCollection {
        let mut collection = CoordinatorUsageCollection::default();
        let live_record = context.live_record.as_ref();
        let launched_model = live_record
            .map(|r| r.model.trim())
            .filter(|m| !m.is_empty())
            .map(str::to_owned);

        let Some(config_dir) = context.resolved_claude_config_dir() else {
            if let Some(rec) = live_record {
                emit_unmeasured_spawn_placeholder(
                    &mut collection,
                    rec,
                    launched_model.as_deref(),
                    context.now_epoch_s,
                    "Claude config directory is unavailable".to_owned(),
                );
            }
            return collection;
        };

        let projects_dir = transcript_projects_dir_in_config(&config_dir, &context.working_directory);
        let session_files = match list_session_transcripts(&projects_dir).await {
            Ok(files) => files,
            Err(ListSessionsError::NotFound) => {
                if let Some(rec) = live_record {
                    emit_unmeasured_spawn_placeholder(
                        &mut collection,
                        rec,
                        launched_model.as_deref(),
                        context.now_epoch_s,
                        format!(
                            "Coordinator transcript directory not found ({})",
                            projects_dir.display()
                        ),
                    );
                }
                return collection;
            }
            Err(ListSessionsError::Unreadable(reason)) => {
                if let Some(rec) = live_record {
                    emit_unmeasured_spawn_placeholder(
                        &mut collection,
                        rec,
                        launched_model.as_deref(),
                        context.now_epoch_s,
                        reason,
                    );
                }
                return collection;
            }
        };

        // Pick which transcript (if any) belongs to the currently live
        // coordinator session: the newest one created at or after the live
        // launch. Creation time, not modification time, so the outgoing
        // session of a handoff (which can still be flushing its last lines
        // when the replacement is spawned) is never mistaken for the new one.
        let active_transcript_path = live_record.and_then(|rec| {
            let spawned_at = rec.spawned_at.unwrap_or(0);
            session_files
                .iter()
                .filter(|entry| match entry.created_epoch_s {
                    Some(created) => created >= spawned_at.saturating_sub(2),
                    None => entry.mtime_epoch_s.unwrap_or(0) >= spawned_at,
                })
                .max_by(|a, b| a.mtime_epoch_s.cmp(&b.mtime_epoch_s).then_with(|| a.path.cmp(&b.path)))
                .map(|entry| entry.path.clone())
        });

        let mut state = self.state.lock().await;
        state.pending.clear();
        let present: HashSet<String> = session_files
            .iter()
            .filter_map(|entry| entry.path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
            .collect();
        collection.listed_session_ids = Some(present.iter().cloned().collect());
        state.sessions.retain(|session_id, _| present.contains(session_id));
        let mut live_covered = false;

        for entry in &session_files {
            let Some(session_id) = entry
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let is_active = active_transcript_path.as_deref() == Some(entry.path.as_path());
            let status = if is_active { "active" } else { "completed" }.to_owned();
            let spawn_token = is_active.then(|| live_record.map(|r| r.spawn_token.clone())).flatten();
            let session_launched_model = if is_active { launched_model.clone() } else { None };
            if is_active {
                collection.active_session_ids.push(session_id.to_owned());
                if let Some(token) = &spawn_token {
                    collection.covered_spawn_tokens.push(token.clone());
                    live_covered = true;
                }
            }

            let (subagent_paths, subagent_dir_error) = match enumerate_subagent_transcripts(&entry.path).await {
                Ok(paths) => (paths, None),
                Err(reason) => (Vec::new(), Some(reason)),
            };
            let fallback_ts = entry.mtime_epoch_s.unwrap_or(context.now_epoch_s);

            let mut capture = state
                .sessions
                .remove(session_id)
                .unwrap_or_else(|| SessionCapture::new(&entry.path));
            if matches!(capture.poll(&subagent_paths, fallback_ts).await, PollOutcome::Rebuild) {
                capture = SessionCapture::new(&entry.path);
                capture.poll(&subagent_paths, fallback_ts).await;
            }

            let gap = capture.gap_reason(subagent_dir_error.as_deref());
            let key = EmitKey {
                progress: capture.progress,
                status: status.clone(),
                spawn_token: spawn_token.clone(),
                launched_model: session_launched_model.clone(),
                gap: gap.clone(),
            };
            if capture.acknowledged.as_ref() != Some(&key) {
                let facts_complete = capture.acknowledged.is_none();
                let snapshot = build_snapshot(
                    SnapshotInput::builder()
                        .session_id(session_id)
                        .status(&status)
                        .maybe_spawn_token(spawn_token.as_deref())
                        .maybe_launched_model(session_launched_model.as_deref())
                        .fallback_ts(fallback_ts)
                        .maybe_gap(gap.as_deref())
                        .build(),
                    &capture.acc,
                    facts_complete,
                    &capture.dirty_messages,
                    &capture.dirty_durations,
                );
                collection.snapshots.push(snapshot);
                state.pending.insert(session_id.to_owned(), key);
            }
            state.sessions.insert(session_id.to_owned(), capture);
        }

        if let Some(rec) = live_record
            && !live_covered
        {
            emit_unmeasured_spawn_placeholder(
                &mut collection,
                rec,
                launched_model.as_deref(),
                context.now_epoch_s,
                format!("Coordinator transcript not found in {}", projects_dir.display()),
            );
        }

        collection
    }
}

fn emit_unmeasured_spawn_placeholder(
    collection: &mut CoordinatorUsageCollection,
    rec: &CoordinatorTmuxRecord,
    launched_model: Option<&str>,
    now_epoch_s: i64,
    reason: String,
) {
    let session_id = format!("{COORDINATOR_UNREADABLE_SPAWN_PREFIX}{}", rec.spawn_token);
    let ts = rec.spawned_at.unwrap_or(now_epoch_s);
    collection.active_session_ids.push(session_id.clone());
    collection.snapshots.push(
        CoordinatorSessionSnapshot::builder()
            .session_id(session_id.clone())
            .spawn_token(rec.spawn_token.clone())
            .slices(vec![
                CoordinatorUsageSlice::builder()
                    .session_id(session_id)
                    .spawn_token(rec.spawn_token.clone())
                    .source(CLAUDE_TRANSCRIPT_SOURCE)
                    .driver(DRIVER_SLUG_CLAUDE)
                    .maybe_launched_model(launched_model.map(str::to_owned))
                    .status("active")
                    .bucket_start_epoch_s(hour_bucket(ts))
                    .first_event_at(ts)
                    .last_event_at(ts)
                    .usage_unavailable(reason)
                    .build(),
            ])
            .full_replacement(true)
            .build(),
    );
}

struct SessionTranscriptFile {
    path: PathBuf,
    mtime_epoch_s: Option<i64>,
    created_epoch_s: Option<i64>,
}

enum ListSessionsError {
    NotFound,
    Unreadable(String),
}

async fn list_session_transcripts(
    projects_dir: &Path,
) -> std::result::Result<Vec<SessionTranscriptFile>, ListSessionsError> {
    let mut entries = match tokio::fs::read_dir(projects_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(ListSessionsError::NotFound),
        Err(error) => {
            return Err(ListSessionsError::Unreadable(format!(
                "Coordinator transcript directory unreadable ({}): {error}",
                projects_dir.display()
            )));
        }
    };
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(|error| {
        ListSessionsError::Unreadable(format!(
            "Coordinator transcript directory unreadable ({}): {error}",
            projects_dir.display()
        ))
    })? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        if file_type.is_dir() {
            continue;
        }
        let metadata = tokio::fs::metadata(&path).await.ok();
        let mtime_epoch_s = metadata
            .as_ref()
            .and_then(|meta| meta.modified().ok())
            .and_then(system_time_epoch_s);
        let created_epoch_s = metadata
            .as_ref()
            .and_then(|meta| meta.created().ok())
            .and_then(system_time_epoch_s);
        files.push(SessionTranscriptFile {
            path,
            mtime_epoch_s,
            created_epoch_s,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn system_time_epoch_s(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

fn hour_bucket(ts: i64) -> i64 {
    ts.div_euclid(HOUR_SECS) * HOUR_SECS
}

#[derive(bon::Builder)]
struct SnapshotInput<'a> {
    session_id: &'a str,
    status: &'a str,
    spawn_token: Option<&'a str>,
    launched_model: Option<&'a str>,
    fallback_ts: i64,
    gap: Option<&'a str>,
}

#[derive(Debug, Clone)]
struct MessageUsageRecord {
    timestamp_epoch_s: i64,
    model: Option<String>,
    usage: Option<MessageUsage>,
}

#[derive(Debug, Clone)]
struct DurationRecord {
    assistant_message_id: Option<String>,
    timestamp_epoch_s: i64,
    model: Option<String>,
    duration_ms: i64,
}

enum DirtyFact {
    Message(String),
    Duration(String),
}

#[derive(Default)]
struct SessionTranscriptAccumulator {
    messages: HashMap<String, MessageUsageRecord>,
    durations: HashMap<String, DurationRecord>,
}

impl SessionTranscriptAccumulator {
    fn ingest_value(
        &mut self,
        file_key: &str,
        value_idx: usize,
        value: &Value,
        fallback_ts: i64,
        tracked: &mut TrackedTail,
    ) -> Option<DirtyFact> {
        match decode_claude_record(value) {
            Some(ClaudeRecord::Assistant {
                message_id,
                timestamp,
                model,
                usage,
            }) => {
                let timestamp_epoch_s = timestamp.unwrap_or(fallback_ts);
                let existing = self
                    .messages
                    .entry(message_id.clone())
                    .or_insert_with(|| MessageUsageRecord {
                        timestamp_epoch_s,
                        model: None,
                        usage: None,
                    });
                existing.timestamp_epoch_s = existing.timestamp_epoch_s.min(timestamp_epoch_s);
                merge_claude_chunk(&mut existing.usage, usage);
                if let Some(model) = model {
                    existing.model = Some(model);
                }
                tracked.last_assistant_id = Some(message_id.clone());
                tracked.last_model = existing.model.clone();
                tracked.last_assistant_ts = Some(existing.timestamp_epoch_s);
                Some(DirtyFact::Message(message_id))
            }
            Some(ClaudeRecord::TurnDuration {
                key,
                timestamp,
                duration_ms,
            }) => {
                let key = key
                    .map(|key| format!("uuid:{key}"))
                    .unwrap_or_else(|| format!("anon:{file_key}:{value_idx}"));
                self.durations.insert(
                    key.clone(),
                    DurationRecord {
                        assistant_message_id: tracked.last_assistant_id.clone(),
                        timestamp_epoch_s: tracked.last_assistant_ts.or(timestamp).unwrap_or(fallback_ts),
                        model: tracked.last_model.clone(),
                        duration_ms,
                    },
                );
                Some(DirtyFact::Duration(key))
            }
            None => None,
        }
    }
}

/// An unmeasured stand-in slice for a session whose read produced no usage.
fn unmeasured_slice(input: &SnapshotInput<'_>, reason: String) -> CoordinatorUsageSlice {
    CoordinatorUsageSlice::builder()
        .session_id(input.session_id)
        .maybe_spawn_token(input.spawn_token.map(str::to_owned))
        .source(CLAUDE_TRANSCRIPT_SOURCE)
        .driver(DRIVER_SLUG_CLAUDE)
        .maybe_launched_model(input.launched_model.map(str::to_owned))
        .status(input.status)
        .bucket_start_epoch_s(hour_bucket(input.fallback_ts))
        .first_event_at(input.fallback_ts)
        .last_event_at(input.fallback_ts)
        .usage_unavailable(reason)
        .build()
}

fn message_fact_slice(input: &SnapshotInput<'_>, msg: &MessageUsageRecord) -> CoordinatorUsageSlice {
    let bucket_start = hour_bucket(msg.timestamp_epoch_s);
    let usage = msg.usage.as_ref();
    let slice_unavailable = if usage.is_some() {
        None
    } else {
        Some(
            input
                .gap
                .map(str::to_owned)
                .unwrap_or_else(|| "Coordinator transcript contains no token usage records".to_owned()),
        )
    };
    CoordinatorUsageSlice::builder()
        .session_id(input.session_id)
        .maybe_spawn_token(input.spawn_token.map(str::to_owned))
        .source(CLAUDE_TRANSCRIPT_SOURCE)
        .driver(DRIVER_SLUG_CLAUDE)
        .maybe_launched_model(input.launched_model.map(str::to_owned))
        .status(input.status)
        .bucket_start_epoch_s(bucket_start)
        .first_event_at(msg.timestamp_epoch_s)
        .last_event_at(msg.timestamp_epoch_s)
        .maybe_model(msg.model.clone())
        .maybe_input_tokens(usage.map(|u| u.input_tokens))
        .maybe_output_tokens(usage.map(|u| u.output_tokens))
        .maybe_cache_creation_tokens(usage.map(|u| u.cache_creation_tokens))
        .maybe_cache_creation_5m_tokens(
            usage.and_then(|u| u.cache_creation_ttl_split_known.then_some(u.cache_creation_5m_tokens)),
        )
        .maybe_cache_creation_1h_tokens(
            usage.and_then(|u| u.cache_creation_ttl_split_known.then_some(u.cache_creation_1h_tokens)),
        )
        .maybe_cache_read_tokens(usage.map(|u| u.cache_read_tokens))
        .rounds(1)
        .maybe_usage_unavailable(slice_unavailable)
        .build()
}

fn duration_fact_slice(input: &SnapshotInput<'_>, dur: &DurationRecord) -> CoordinatorUsageSlice {
    let bucket_start = hour_bucket(dur.timestamp_epoch_s);
    CoordinatorUsageSlice::builder()
        .maybe_assistant_message_id(dur.assistant_message_id.clone())
        .session_id(input.session_id)
        .maybe_spawn_token(input.spawn_token.map(str::to_owned))
        .source(CLAUDE_TRANSCRIPT_SOURCE)
        .driver(DRIVER_SLUG_CLAUDE)
        .maybe_launched_model(input.launched_model.map(str::to_owned))
        .status(input.status)
        .bucket_start_epoch_s(bucket_start)
        .first_event_at(dur.timestamp_epoch_s)
        .last_event_at(dur.timestamp_epoch_s)
        .maybe_model(dur.model.clone())
        .agent_active_ms(dur.duration_ms)
        .build()
}

fn build_snapshot(
    input: SnapshotInput<'_>,
    acc: &SessionTranscriptAccumulator,
    facts_complete: bool,
    dirty_messages: &HashSet<String>,
    dirty_durations: &HashSet<String>,
) -> CoordinatorSessionSnapshot {
    let mut facts = BTreeMap::new();
    if facts_complete {
        for (key, message) in &acc.messages {
            facts.insert(format!("message:{key}"), message_fact_slice(&input, message));
        }
        for (key, duration) in &acc.durations {
            facts.insert(format!("duration:{key}"), duration_fact_slice(&input, duration));
        }
    } else {
        for key in dirty_messages {
            if let Some(message) = acc.messages.get(key) {
                facts.insert(format!("message:{key}"), message_fact_slice(&input, message));
            }
        }
        for key in dirty_durations {
            if let Some(duration) = acc.durations.get(key) {
                facts.insert(format!("duration:{key}"), duration_fact_slice(&input, duration));
            }
        }
    }

    let slices = if acc.messages.is_empty() && acc.durations.is_empty() {
        let reason = input
            .gap
            .map(str::to_owned)
            .unwrap_or_else(|| "Coordinator transcript contains no token usage records".to_owned());
        vec![unmeasured_slice(&input, reason)]
    } else {
        Vec::new()
    };

    CoordinatorSessionSnapshot::builder()
        .session_id(input.session_id)
        .maybe_spawn_token(input.spawn_token.map(str::to_owned))
        .maybe_launched_model(input.launched_model.map(str::to_owned))
        .status(input.status)
        .slices(slices)
        .full_replacement(false)
        .maybe_capture_gap(input.gap.map(str::to_owned))
        .facts(facts)
        .facts_complete(facts_complete)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::COORDINATOR_USAGE_KIND;
    use tempfile::TempDir;

    fn live_record(token: &str, model: &str, spawned_at: i64) -> CoordinatorTmuxRecord {
        CoordinatorTmuxRecord {
            session_name: "boss-coordinator".to_owned(),
            spawn_token: token.to_owned(),
            spawn_state: "created".to_owned(),
            model: model.to_owned(),
            launched_claude_version: None,
            spawned_at: Some(spawned_at),
            pane_id: None,
            liveness_passed_at: None,
        }
    }

    pub(super) fn setup_projects_dir(dir: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let working_dir = dir.path().join("boss-session");
        let claude_config = dir.path().join("claude-config");
        let projects_dir = transcript_projects_dir_in_config(&claude_config, &working_dir);
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&projects_dir).unwrap();
        (working_dir, claude_config, projects_dir)
    }

    #[tokio::test]
    async fn captures_parent_and_subagents_per_hour_and_model_without_double_counting() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = TempDir::new().unwrap();
        let (working_dir, claude_config, projects_dir) = setup_projects_dir(&dir);

        // Hour 1: 2026-11-25T10:00:00Z = 1795600800
        // Hour 2: 2026-11-25T11:00:00Z = 1795604400
        let parent_path = projects_dir.join("sess-alpha.jsonl");
        let subagents_dir = projects_dir.join("sess-alpha").join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();

        std::fs::write(
            &parent_path,
            concat!(
                // Streaming chunk 1 for msg-1 (partial output)
                r#"{"type":"assistant","timestamp":"2026-11-25T10:05:00Z","message":{"id":"msg-1","model":"claude-opus-4-6","usage":{"input_tokens":100,"output_tokens":2,"cache_creation_input_tokens":50,"cache_read_input_tokens":1000,"cache_creation":{"ephemeral_5m_input_tokens":50,"ephemeral_1h_input_tokens":0}}}}"#,
                "\n",
                // Streaming chunk 2 for msg-1 (final output replaces chunk 1)
                r#"{"type":"assistant","timestamp":"2026-11-25T10:05:05Z","message":{"id":"msg-1","model":"claude-opus-4-6","usage":{"input_tokens":100,"output_tokens":25,"cache_creation_input_tokens":50,"cache_read_input_tokens":1000,"cache_creation":{"ephemeral_5m_input_tokens":50,"ephemeral_1h_input_tokens":0}}}}"#,
                "\n",
                // Split chunk without usage must not erase msg-1 usage
                r#"{"type":"assistant","timestamp":"2026-11-25T10:05:06Z","message":{"id":"msg-1","model":"claude-opus-4-6"}}"#,
                "\n",
                // Turn duration in hour 1
                r#"{"type":"system","subtype":"turn_duration","uuid":"td-1","timestamp":"2026-11-25T10:05:10Z","durationMs":4500}"#,
                "\n",
                // Message in hour 2
                r#"{"type":"assistant","timestamp":"2026-11-25T11:15:00Z","message":{"id":"msg-2","model":"claude-opus-4-6","usage":{"input_tokens":200,"output_tokens":40,"cache_creation_input_tokens":100,"cache_read_input_tokens":2000}}}"#,
                "\n",
                // Mid-write partial line without trailing newline must be ignored cleanly
                r#"{"type":"assistant","timestamp":"2026-11-25T11:16:00Z","message":{"id":"partial""#,
            ),
        )
        .unwrap();

        // Subagent uses Haiku in hour 1 and repeats msg-1 (must deduplicate!)
        std::fs::write(
            subagents_dir.join("agent-explore.jsonl"),
            concat!(
                r#"{"type":"assistant","timestamp":"2026-11-25T10:05:05Z","message":{"id":"msg-1","model":"claude-opus-4-6","usage":{"input_tokens":100,"output_tokens":25,"cache_creation_input_tokens":50,"cache_read_input_tokens":1000}}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-11-25T10:12:00Z","message":{"id":"msg-sub-1","model":"claude-haiku-4-5","usage":{"input_tokens":60,"output_tokens":15,"cache_creation_input_tokens":10,"cache_read_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":0}}}}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","uuid":"td-sub-1","timestamp":"2026-11-25T10:12:05Z","durationMs":1200}"#,
                "\n",
            ),
        )
        .unwrap();

        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        let ctx = CoordinatorUsageContext::builder()
            .working_directory(working_dir)
            .claude_config_dir(claude_config)
            .live_record(live_record("tok-alpha", "opus", 1_795_514_000))
            .now_epoch_s(1_795_520_000)
            .build();

        let written = recorder.sync(&db, &ctx).await.unwrap();
        assert_eq!(written, 3, "hour1 opus, hour1 haiku subagent, hour2 opus");

        // Re-syncing unchanged files is a no-op and never double counts.
        let written_again = recorder.sync(&db, &ctx).await.unwrap();
        assert_eq!(written_again, 0);

        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|r| r.execution_kind == COORDINATOR_USAGE_KIND));
        assert!(records.iter().all(|r| r.driver.as_deref() == Some("claude")));

        let sum_in: i64 = records.iter().filter_map(|r| r.input_tokens).sum();
        let sum_out: i64 = records.iter().filter_map(|r| r.output_tokens).sum();
        let sum_cw: i64 = records.iter().filter_map(|r| r.cache_creation_tokens).sum();
        let sum_cr: i64 = records.iter().filter_map(|r| r.cache_read_tokens).sum();
        let sum_ms: i64 = records.iter().filter_map(|r| r.agent_active_ms).sum();
        assert_eq!(sum_in, 100 + 60 + 200);
        assert_eq!(sum_out, 25 + 15 + 40);
        assert_eq!(sum_cw, 50 + 10 + 100);
        assert_eq!(sum_cr, 1000 + 300 + 2000);
        assert_eq!(sum_ms, 4500 + 1200);

        // Window query scoped to hour 2 returns only hour 2's slice.
        let hour2 = db.cost_records_for_window(1_795_604_400, 1_795_608_000).unwrap();
        assert_eq!(hour2.len(), 1);
        assert_eq!(hour2[0].input_tokens, Some(200));
        assert_eq!(hour2[0].model.as_deref(), Some("claude-opus-4-6"));
    }

    #[tokio::test]
    async fn rereading_after_engine_restart_does_not_double_count() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = TempDir::new().unwrap();
        let (working_dir, claude_config, projects_dir) = setup_projects_dir(&dir);

        std::fs::write(
            projects_dir.join("sess-restart.jsonl"),
            concat!(
                r#"{"type":"assistant","timestamp":"2026-11-25T10:05:00Z","message":{"id":"m1","model":"claude-opus-4-6","usage":{"input_tokens":500,"output_tokens":100,"cache_creation_input_tokens":20,"cache_read_input_tokens":4000}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let ctx = CoordinatorUsageContext::builder()
            .working_directory(working_dir)
            .claude_config_dir(claude_config)
            .live_record(live_record("tok-1", "opus", 1_795_514_000))
            .now_epoch_s(1_795_515_000)
            .build();

        let recorder_before_restart = CoordinatorUsageRecorder::for_claude_transcripts();
        recorder_before_restart.sync(&db, &ctx).await.unwrap();

        // Brand-new recorder with empty in-memory cache simulates an engine restart.
        let recorder_after_restart = CoordinatorUsageRecorder::for_claude_transcripts();
        recorder_after_restart.sync(&db, &ctx).await.unwrap();

        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].input_tokens, Some(500));
        assert_eq!(records[0].output_tokens, Some(100));
        assert_eq!(records[0].cache_creation_tokens, Some(20));
        assert_eq!(records[0].cache_read_tokens, Some(4000));
    }

    #[tokio::test]
    async fn usage_survives_coordinator_restart_handoff_and_transcript_deletion() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = TempDir::new().unwrap();
        let (working_dir, claude_config, projects_dir) = setup_projects_dir(&dir);

        let sess1 = projects_dir.join("sess-1.jsonl");
        std::fs::write(
            &sess1,
            concat!(
                r#"{"type":"assistant","timestamp":"2026-11-25T10:05:00Z","message":{"id":"s1-m1","model":"claude-opus-4-6","usage":{"input_tokens":300,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":1000}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        let ctx1 = CoordinatorUsageContext::builder()
            .working_directory(working_dir.clone())
            .claude_config_dir(claude_config.clone())
            .live_record(live_record("tok-1", "opus", 1_795_514_000))
            .now_epoch_s(1_795_515_000)
            .build();
        recorder.sync(&db, &ctx1).await.unwrap();

        // Coordinator restarts/hands off to session 2, and old transcript is removed.
        std::fs::remove_file(&sess1).unwrap();
        let sess2 = projects_dir.join("sess-2.jsonl");
        std::fs::write(
            &sess2,
            concat!(
                r#"{"type":"assistant","timestamp":"2026-11-25T12:10:00Z","message":{"id":"s2-m1","model":"claude-opus-4-6","usage":{"input_tokens":700,"output_tokens":150,"cache_creation_input_tokens":40,"cache_read_input_tokens":2500}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let ctx2 = CoordinatorUsageContext::builder()
            .working_directory(working_dir)
            .claude_config_dir(claude_config)
            .live_record(live_record("tok-2", "opus", 1_795_521_000))
            .now_epoch_s(1_795_522_000)
            .build();
        recorder.sync(&db, &ctx2).await.unwrap();

        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.len(), 2, "both pre-handoff and post-handoff sessions persist");
        assert_eq!(records[0].run_status, "completed");
        assert_eq!(records[0].input_tokens, Some(300));
        assert_eq!(records[1].run_status, "active");
        assert_eq!(records[1].input_tokens, Some(700));
    }

    #[tokio::test]
    async fn unreadable_or_missing_transcript_records_unknown_never_zero() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = TempDir::new().unwrap();
        let working_dir = dir.path().join("boss-session");
        let claude_config = dir.path().join("claude-config");
        std::fs::create_dir_all(&working_dir).unwrap();

        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        let ctx = CoordinatorUsageContext::builder()
            .working_directory(working_dir.clone())
            .claude_config_dir(claude_config.clone())
            .live_record(live_record("tok-missing", "opus", 1_795_514_000))
            .now_epoch_s(1_795_514_500)
            .build();

        // Projects dir does not exist yet -> records unknown placeholder, never zero.
        recorder.sync(&db, &ctx).await.unwrap();
        let missing = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].input_tokens, None);
        assert_eq!(missing[0].output_tokens, None);
        assert!(
            missing[0]
                .usage_unavailable
                .as_deref()
                .is_some_and(|r| r.contains("not found"))
        );

        // Corrupt JSON transcript -> records unknown with parse reason, retires placeholder.
        let projects_dir = transcript_projects_dir_in_config(&claude_config, &working_dir);
        std::fs::create_dir_all(&projects_dir).unwrap();
        std::fs::write(projects_dir.join("sess-corrupt.jsonl"), "not valid json\n").unwrap();
        recorder.sync(&db, &ctx).await.unwrap();
        let corrupt = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(corrupt.len(), 1);
        assert_eq!(corrupt[0].input_tokens, None);
        assert!(
            corrupt[0]
                .usage_unavailable
                .as_deref()
                .is_some_and(|r| r.contains("invalid JSON"))
        );
    }

    fn assistant_line(id: &str, ts: &str, input: i64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","model":"claude-opus-4-6","usage":{{"input_tokens":{input},"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
        ) + "\n"
    }

    fn window_totals(db: &WorkDb) -> (Option<i64>, Option<String>) {
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        (
            records.iter().filter_map(|r| r.input_tokens).reduce(|a, b| a + b),
            records.iter().find_map(|r| r.usage_unavailable.clone()),
        )
    }

    fn context(working_dir: &Path, claude_config: &Path) -> CoordinatorUsageContext {
        CoordinatorUsageContext::builder()
            .working_directory(working_dir.to_owned())
            .claude_config_dir(claude_config.to_owned())
            .live_record(live_record("tok-gap", "opus", 1_795_514_000))
            .now_epoch_s(1_795_520_000)
            .build()
    }

    #[tokio::test]
    async fn a_corrupt_line_is_skipped_and_never_discards_good_lines_or_stored_usage() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = TempDir::new().unwrap();
        let (working_dir, claude_config, projects_dir) = setup_projects_dir(&dir);
        let path = projects_dir.join("sess-gap.jsonl");
        std::fs::write(
            &path,
            assistant_line("m1", "2026-11-25T10:05:00Z", 100)
                + "this is not json\n"
                + &assistant_line("m2", "2026-11-25T10:06:00Z", 200),
        )
        .unwrap();

        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        let ctx = context(&working_dir, &claude_config);
        recorder.sync(&db, &ctx).await.unwrap();
        let (input, reason) = window_totals(&db);
        assert_eq!(input, Some(300), "good, corrupt, good: both good messages are counted");
        assert!(reason.is_some_and(|r| r.contains("1 malformed transcript line(s) skipped")));

        // Later corruption of the same file keeps everything already captured.
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{broken\n").unwrap();
        file.write_all(assistant_line("m3", "2026-11-25T10:07:00Z", 50).as_bytes())
            .unwrap();
        drop(file);
        recorder.sync(&db, &ctx).await.unwrap();
        let (input, reason) = window_totals(&db);
        assert_eq!(input, Some(350));
        assert!(reason.is_some_and(|r| r.contains("2 malformed transcript line(s) skipped")));

        // After an engine restart the whole file is re-read: same totals.
        let restarted = CoordinatorUsageRecorder::for_claude_transcripts();
        restarted.sync(&db, &ctx).await.unwrap();
        assert_eq!(window_totals(&db).0, Some(350));
    }

    #[tokio::test]
    async fn a_removed_subagent_file_keeps_known_totals_and_marks_the_session_incomplete() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = TempDir::new().unwrap();
        let (working_dir, claude_config, projects_dir) = setup_projects_dir(&dir);
        std::fs::write(
            projects_dir.join("sess-sub.jsonl"),
            assistant_line("p1", "2026-11-25T10:05:00Z", 100),
        )
        .unwrap();
        let subagents = projects_dir.join("sess-sub").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let sub_file = subagents.join("agent-a.jsonl");
        std::fs::write(&sub_file, assistant_line("s1", "2026-11-25T10:06:00Z", 40)).unwrap();

        let ctx = context(&working_dir, &claude_config);
        CoordinatorUsageRecorder::for_claude_transcripts()
            .sync(&db, &ctx)
            .await
            .unwrap();
        assert_eq!(window_totals(&db), (Some(140), None));

        // The subagent transcript disappears before the next engine run.
        std::fs::remove_file(&sub_file).unwrap();
        CoordinatorUsageRecorder::for_claude_transcripts()
            .sync(&db, &ctx)
            .await
            .unwrap();
        let (input, reason) = window_totals(&db);
        assert_eq!(input, Some(140), "known usage is kept");
        assert!(reason.is_some(), "the shrunken read is marked incomplete");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_parent_transcript_keeps_stored_usage() {
        use std::os::unix::fs::PermissionsExt;

        let (_db_dir, db) = crate::test_support::open_db();
        let dir = TempDir::new().unwrap();
        let (working_dir, claude_config, projects_dir) = setup_projects_dir(&dir);
        let path = projects_dir.join("sess-perm.jsonl");
        std::fs::write(&path, assistant_line("m1", "2026-11-25T10:05:00Z", 100)).unwrap();
        let ctx = context(&working_dir, &claude_config);
        CoordinatorUsageRecorder::for_claude_transcripts()
            .sync(&db, &ctx)
            .await
            .unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            return; // running with privileges that ignore file modes
        }
        CoordinatorUsageRecorder::for_claude_transcripts()
            .sync(&db, &ctx)
            .await
            .unwrap();
        let (input, reason) = window_totals(&db);
        assert_eq!(input, Some(100), "an unreadable reread never deletes measured usage");
        assert!(reason.is_some_and(|r| r.contains("unreadable")));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[tokio::test]
    async fn a_failed_write_is_retried_on_the_next_sync_not_stranded() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = TempDir::new().unwrap();
        let (working_dir, claude_config, projects_dir) = setup_projects_dir(&dir);
        std::fs::write(
            projects_dir.join("sess-a.jsonl"),
            assistant_line("a1", "2026-11-25T10:05:00Z", 10),
        )
        .unwrap();
        std::fs::write(
            projects_dir.join("sess-b.jsonl"),
            assistant_line("b1", "2026-11-25T10:05:00Z", 20),
        )
        .unwrap();
        std::fs::write(
            projects_dir.join("sess-c.jsonl"),
            assistant_line("c1", "2026-11-25T10:05:00Z", 30),
        )
        .unwrap();
        db.connect()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER block_sess_b BEFORE INSERT ON coordinator_usage_slices
                 WHEN NEW.session_id = 'sess-b'
                 BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
            )
            .unwrap();

        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        let ctx = context(&working_dir, &claude_config);
        assert!(recorder.sync(&db, &ctx).await.is_err());
        assert_eq!(
            window_totals(&db).0,
            Some(40),
            "the sessions around the failing one are still persisted"
        );

        db.connect()
            .unwrap()
            .execute_batch("DROP TRIGGER block_sess_b;")
            .unwrap();
        // The transcripts did not change, yet the unpersisted session is retried.
        let written = recorder.sync(&db, &ctx).await.unwrap();
        assert_eq!(written, 1, "only the stranded session is written again");
        assert_eq!(window_totals(&db).0, Some(60));
    }

    #[tokio::test]
    async fn the_outgoing_session_is_not_tagged_with_the_new_launch() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = TempDir::new().unwrap();
        let (working_dir, claude_config, projects_dir) = setup_projects_dir(&dir);
        std::fs::write(
            projects_dir.join("sess-outgoing.jsonl"),
            assistant_line("o1", "2026-11-25T10:05:00Z", 10),
        )
        .unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        // The replacement launch was spawned after the outgoing transcript was
        // created, and has not written a transcript of its own yet.
        let ctx = CoordinatorUsageContext::builder()
            .working_directory(working_dir)
            .claude_config_dir(claude_config)
            .live_record(live_record("tok-new", "opus", now + 600))
            .now_epoch_s(now)
            .build();
        CoordinatorUsageRecorder::for_claude_transcripts()
            .sync(&db, &ctx)
            .await
            .unwrap();
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        let outgoing = records.iter().find(|r| r.input_tokens == Some(10)).unwrap();
        assert_eq!(outgoing.run_status, "completed");
        assert!(
            records
                .iter()
                .any(|r| r.input_tokens.is_none()
                    && r.usage_unavailable.as_deref().is_some_and(|u| u.contains("not found"))),
            "the new launch still shows its unknown placeholder"
        );
    }

    struct StubAcpSource;

    #[async_trait::async_trait]
    impl CoordinatorUsageSource for StubAcpSource {
        fn source_name(&self) -> &'static str {
            "acp_usage_update"
        }

        async fn collect(&self, _context: &CoordinatorUsageContext) -> CoordinatorUsageCollection {
            CoordinatorUsageCollection {
                snapshots: vec![
                    CoordinatorSessionSnapshot::builder()
                        .session_id("acp-sess-1")
                        .slices(vec![
                            CoordinatorUsageSlice::builder()
                                .session_id("acp-sess-1")
                                .source(self.source_name())
                                .driver("claude")
                                .bucket_start_epoch_s(1_795_514_400)
                                .first_event_at(1_795_514_500)
                                .last_event_at(1_795_514_900)
                                .model("claude-opus-4-6")
                                .input_tokens(420)
                                .output_tokens(80)
                                .cache_creation_tokens(10)
                                .cache_read_tokens(900)
                                .rounds(2)
                                .build(),
                        ])
                        .full_replacement(false)
                        .build(),
                ],
                active_session_ids: vec!["acp-sess-1".to_owned()],
                covered_spawn_tokens: Vec::new(),
                listed_session_ids: None,
            }
        }
    }

    #[tokio::test]
    async fn custom_source_interface_feeds_same_storage_and_cost_queries() {
        let (_db_dir, db) = crate::test_support::open_db();
        let recorder = CoordinatorUsageRecorder::new(Arc::new(StubAcpSource));
        assert_eq!(recorder.source_name(), "acp_usage_update");
        let ctx = CoordinatorUsageContext::builder()
            .working_directory(PathBuf::from("/unused"))
            .now_epoch_s(1_795_515_000)
            .build();
        recorder.sync(&db, &ctx).await.unwrap();

        let records = db.usage_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.runs.len(), 1);
        assert_eq!(records.runs[0].execution_kind, COORDINATOR_USAGE_KIND);
        assert_eq!(records.runs[0].input_tokens, Some(420));
        assert_eq!(records.runs[0].output_tokens, Some(80));
    }
}

#[cfg(test)]
mod persistence_regressions {
    use super::tests::setup_projects_dir;
    use super::*;
    use std::io::Write;

    fn append(path: &Path, id: &str, timestamp: &str, tokens: i64) {
        let value = serde_json::json!({"type":"assistant", "timestamp":timestamp,
            "message":{"id":id,"model":"claude-opus-4-6","usage":{"input_tokens":tokens,
            "output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}});
        writeln!(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap(),
            "{value}"
        )
        .unwrap();
    }

    #[tokio::test]
    async fn missing_subagent_then_restart_and_same_and_later_hour_appends() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = tempfile::TempDir::new().unwrap();
        let (working, config, projects) = setup_projects_dir(&dir);
        let parent = projects.join("session.jsonl");
        let subdir = projects.join("session/subagents");
        std::fs::create_dir_all(&subdir).unwrap();
        let sub = subdir.join("agent.jsonl");
        append(&parent, "parent", "2026-11-25T10:05:00Z", 100);
        append(&sub, "subagent", "2026-11-25T10:06:00Z", 700);
        let context = CoordinatorUsageContext::builder()
            .working_directory(working)
            .claude_config_dir(config)
            .now_epoch_s(1795608000)
            .build();
        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        recorder.sync(&db, &context).await.unwrap();
        std::fs::remove_file(&sub).unwrap();
        recorder.sync(&db, &context).await.unwrap();
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.iter().filter_map(|r| r.input_tokens).sum::<i64>(), 800);
        assert!(records.iter().all(|r| r.usage_unavailable.is_some()));
        drop(recorder);
        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        append(&parent, "new-same-hour", "2026-11-25T10:30:00Z", 200);
        recorder.sync(&db, &context).await.unwrap();
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.iter().filter_map(|r| r.input_tokens).sum::<i64>(), 1000);
        assert!(records.iter().all(|r| r.usage_unavailable.is_some()));
        append(&parent, "new-later-hour", "2026-11-25T11:05:00Z", 300);
        recorder.sync(&db, &context).await.unwrap();
        recorder.sync(&db, &context).await.unwrap();
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records.iter().filter_map(|r| r.input_tokens).sum::<i64>(), 1300);
        // Only the hour containing the missing subagent carries the gap; the
        // later hour whose capture is complete has no gap.
        assert!(records[0].usage_unavailable.is_some());
        assert_eq!(records[1].usage_unavailable, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_parent_then_readable_again_keeps_counts_and_clears_gap() {
        use std::os::unix::fs::PermissionsExt;

        let (_db_dir, db) = crate::test_support::open_db();
        let dir = tempfile::TempDir::new().unwrap();
        let (working, config, projects) = setup_projects_dir(&dir);
        let parent = projects.join("sess-transient.jsonl");
        append(&parent, "m1", "2026-11-25T10:05:00Z", 150);
        let context = CoordinatorUsageContext::builder()
            .working_directory(working)
            .claude_config_dir(config)
            .now_epoch_s(1795608000)
            .build();
        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        recorder.sync(&db, &context).await.unwrap();
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].input_tokens, Some(150));
        assert_eq!(records[0].usage_unavailable, None);

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&parent).is_ok() {
            return; // running with privileges that ignore file modes
        }
        recorder.sync(&db, &context).await.unwrap();
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].input_tokens, Some(150));
        assert!(
            records[0]
                .usage_unavailable
                .as_deref()
                .is_some_and(|r| r.contains("unreadable"))
        );

        // Once readable again, the transient gap clears and counts are kept.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o600)).unwrap();
        recorder.sync(&db, &context).await.unwrap();
        let records = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].input_tokens, Some(150));
        assert_eq!(records[0].usage_unavailable, None);
    }

    #[tokio::test]
    async fn handoff_preserves_outgoing_session_launched_model_and_spawn_token() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = tempfile::TempDir::new().unwrap();
        let (working, config, projects) = setup_projects_dir(&dir);
        let outgoing = projects.join("sess-old.jsonl");
        append(&outgoing, "m1", "2026-11-25T10:05:00Z", 200);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        let ctx1 = CoordinatorUsageContext::builder()
            .working_directory(working.clone())
            .claude_config_dir(config.clone())
            .live_record(CoordinatorTmuxRecord {
                session_name: "boss-coordinator".to_owned(),
                spawn_token: "tok-old".to_owned(),
                spawn_state: "created".to_owned(),
                model: "opus".to_owned(),
                launched_claude_version: None,
                spawned_at: Some(now - 600),
                pane_id: None,
                liveness_passed_at: None,
            })
            .now_epoch_s(now)
            .build();
        recorder.sync(&db, &ctx1).await.unwrap();

        // Handoff to a new coordinator session while the outgoing transcript remains on disk.
        let ctx2 = CoordinatorUsageContext::builder()
            .working_directory(working)
            .claude_config_dir(config)
            .live_record(CoordinatorTmuxRecord {
                session_name: "boss-coordinator".to_owned(),
                spawn_token: "tok-new".to_owned(),
                spawn_state: "created".to_owned(),
                model: "sonnet".to_owned(),
                launched_claude_version: None,
                spawned_at: Some(now + 600),
                pane_id: None,
                liveness_passed_at: None,
            })
            .now_epoch_s(now + 605)
            .build();
        recorder.sync(&db, &ctx2).await.unwrap();

        let usage = db.usage_records_for_window(0, i64::MAX).unwrap();
        let old_run = usage
            .runs
            .iter()
            .find(|r| r.execution_id.ends_with("sess-old"))
            .expect("outgoing session slice");
        assert_eq!(old_run.execution_status, "completed");
        assert_eq!(old_run.execution_model.as_deref(), Some("opus"));

        let spawn_token: Option<String> = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT spawn_token FROM coordinator_usage_slices WHERE session_id = 'sess-old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(spawn_token.as_deref(), Some("tok-old"));
    }

    #[tokio::test]
    async fn prunes_facts_and_capture_state_for_completed_unlisted_sessions_while_keeping_slices() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = tempfile::TempDir::new().unwrap();
        let (working, config, projects) = setup_projects_dir(&dir);
        let old_path = projects.join("sess-pruned.jsonl");
        append(&old_path, "m1", "2026-11-25T10:05:00Z", 300);
        let context = CoordinatorUsageContext::builder()
            .working_directory(working)
            .claude_config_dir(config)
            .now_epoch_s(1795608000)
            .build();
        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        recorder.sync(&db, &context).await.unwrap();

        let facts_before: i64 = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM coordinator_usage_facts WHERE session_id = 'sess-pruned'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(facts_before, 1);

        // Claude Code cleans up the old transcript file.
        std::fs::remove_file(&old_path).unwrap();
        recorder.sync(&db, &context).await.unwrap();

        let conn = db.connect().unwrap();
        let facts_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM coordinator_usage_facts WHERE session_id = 'sess-pruned'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let state_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM coordinator_usage_capture_state WHERE session_id = 'sess-pruned'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(facts_after, 0, "facts for unlisted completed session must be pruned");
        assert_eq!(
            state_after, 0,
            "capture state for unlisted completed session must be pruned"
        );

        let slices = db.cost_records_for_window(0, i64::MAX).unwrap();
        assert_eq!(slices.len(), 1, "durable coordinator_usage_slices must survive pruning");
        assert_eq!(slices[0].input_tokens, Some(300));
    }

    #[tokio::test]
    async fn non_usage_transcript_lines_do_not_trigger_db_writes() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = tempfile::TempDir::new().unwrap();
        let (working, config, projects) = setup_projects_dir(&dir);
        let parent = projects.join("sess-quiet.jsonl");
        append(&parent, "m1", "2026-11-25T10:05:00Z", 100);
        let context = CoordinatorUsageContext::builder()
            .working_directory(working)
            .claude_config_dir(config)
            .now_epoch_s(1795608000)
            .build();
        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        assert_eq!(recorder.sync(&db, &context).await.unwrap(), 1);

        // Appending user and tool_result lines must not trigger a slice write.
        let mut file = std::fs::OpenOptions::new().append(true).open(&parent).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"2026-11-25T10:06:00Z","message":{{"role":"user","content":"hi"}}}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"type":"progress","timestamp":"2026-11-25T10:06:01Z"}}"#).unwrap();
        drop(file);

        assert_eq!(recorder.sync(&db, &context).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn gapped_streaming_chunks_and_closing_duration_stay_in_original_hour() {
        let (_db_dir, db) = crate::test_support::open_db();
        let dir = tempfile::TempDir::new().unwrap();
        let (working, config, projects) = setup_projects_dir(&dir);
        let parent = projects.join("session.jsonl");
        append(&parent, "message", "2026-11-25T10:59:59Z", 100);
        let context = CoordinatorUsageContext::builder()
            .working_directory(working)
            .claude_config_dir(config)
            .now_epoch_s(1795608000)
            .build();
        CoordinatorUsageRecorder::for_claude_transcripts()
            .sync(&db, &context)
            .await
            .unwrap();
        // A truncated source after restart contains only the later chunk.
        std::fs::write(&parent, "").unwrap();
        append(&parent, "message", "2026-11-25T11:00:01Z", 200);
        let duration = serde_json::json!({"type":"system","subtype":"turn_duration",
            "uuid":"duration","timestamp":"2026-11-25T11:00:02Z","durationMs":4000});
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&parent).unwrap(),
            "{duration}"
        )
        .unwrap();
        let recorder = CoordinatorUsageRecorder::for_claude_transcripts();
        recorder.sync(&db, &context).await.unwrap();
        let rows = db.cost_records_for_window(1795600800, 1795604400).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, Some(200));
        assert_eq!(rows[0].agent_active_ms, Some(4000));
        assert!(db.cost_records_for_window(1795604400, 1795608000).unwrap().is_empty());
        append(&parent, "next", "2026-11-25T11:59:59Z", 300);
        recorder.sync(&db, &context).await.unwrap();
        let duration = serde_json::json!({"type":"system","subtype":"turn_duration",
            "uuid":"next-duration","timestamp":"2026-11-25T12:00:02Z","durationMs":5000});
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&parent).unwrap(),
            "{duration}"
        )
        .unwrap();
        recorder.sync(&db, &context).await.unwrap();
        let rows = db.cost_records_for_window(1795604400, 1795608000).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_active_ms, Some(5000));
        assert!(db.cost_records_for_window(1795608000, i64::MAX).unwrap().is_empty());
    }
}
