//! Always-on, cheap engine-side wall-clock instrumentation for the
//! `GetWorkTree` task-population RPC — the one request that repopulates
//! the macOS kanban on cold start and product switch.
//!
//! # Motivation
//!
//! The app-side diagnostics from PR #1697 proved the
//! client is not the bottleneck: for the ~1,949-item Boss product the
//! `request` segment (RPC issue → reply off the socket) is ~7.1s p50 while
//! decode+apply+render total ~0.6s. But that `request` segment was a black
//! box — the app measures the engine as opaque. This module decomposes the
//! engine's contribution to that segment into per-stage wall clock:
//!
//! ```text
//!   decode           request envelope deserialization
//!   db.product       one SELECT per named DB query …
//!   db.projects
//!   db.tasks
//!   db.chores
//!   db.task_runtimes N+1 hydration — carries db_queries (statements run)
//!   db.dependencies
//!   db.ai_reviewing
//!   db.ai_review_state
//!   db.doc_pointers  per-task doc-pointer resolution (N+1)
//!   assemble         in-memory projection / flag attachment
//!   queue_wait       handler enqueue → writer-task dequeue (session backlog)
//!   serialize        serde_json of the whole WorkTree response
//!   socket_write     bytes written + flushed to the Unix socket
//!   total            line receipt → last byte flushed (the whole window)
//! ```
//!
//! Every segment gets `duration_ms`; DB segments additionally carry `rows`
//! (rows returned / items iterated) and, for the N+1 stages, `db_queries`
//! (the number of SQL statements executed) so a per-row subquery fan-out is
//! unmistakable in the log.
//!
//! # Output
//!
//! One JSON object per segment, appended to a day-rotated file alongside
//! the app-side one:
//!
//!   `<boss-state-root>/diagnostics/engine-population-timing-YYYY-MM-DD.jsonl`
//!
//! (`<boss-state-root>` = `~/Library/Application Support/Boss`, overridable
//! via `BOSS_ENGINE_DIAGNOSTICS_DIR`; a test-fixture engine gets a directory
//! beside its other files instead — see
//! `IsolationPaths::resolve_diagnostics_dir`.) The directory is resolved once
//! at startup and installed with [`install_global`]. Rotation,
//! retention, and the background-writer task are the generic machinery in
//! the generic `boss_engine_day_rotated_log` crate, shared with `crate::ipc_log`.
//!
//! # Correlation
//!
//! Each event carries the engine-side envelope `request_id` (unique per
//! fetch, always present) plus the app's `fetch_seq` when the app sent one
//! (see `FrontendRequest::GetWorkTree`). App-side lines carry
//! `(product_id, fetch_seq)`, so the two sides join on that pair; the
//! `request_id` groups an engine trace's own segments together.
//!
//! # Cost
//!
//! Timing is a handful of `Instant::now()` reads and a `Vec` of ~12 tiny
//! records per fetch; emission is a non-blocking channel send to a
//! background writer task. Nothing runs unless a `GetWorkTree` is served.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use serde::Serialize;

use boss_engine_day_rotated_log::DayRotatedLogger;

use crate::population_progress::{ProgressReporter, ProgressSink, SLOW_POPULATION_MS, SlowPopulation};

/// Environment override for the diagnostics directory. When set (and
/// non-empty after trimming) the day files are written under this path
/// instead of `<boss-state-root>/diagnostics`. Used by local runs against
/// a scratch install and by tests, which must not touch the real
/// `~/Library/Application Support/Boss`.
pub const DIAGNOSTICS_DIR_ENV: &str = "BOSS_ENGINE_DIAGNOSTICS_DIR";

/// Segment names for the `segment` field. Constants so emit sites can't
/// drift; grouped by phase.
pub mod segment {
    /// Request envelope deserialization (reader loop).
    pub const DECODE: &str = "decode";
    /// `SELECT` for the product row.
    pub const DB_PRODUCT: &str = "db.product";
    /// `SELECT` for the product's projects.
    pub const DB_PROJECTS: &str = "db.projects";
    /// `SELECT` for the product's tasks (project_task/design/investigation/revision).
    pub const DB_TASKS: &str = "db.tasks";
    /// `SELECT` for the product's chores/followups.
    pub const DB_CHORES: &str = "db.chores";
    /// Per-item runtime hydration — the primary N+1. `db_queries` exposes
    /// the per-row subquery fan-out.
    pub const DB_TASK_RUNTIMES: &str = "db.task_runtimes";
    /// `SELECT` for the product's cross-item dependencies.
    pub const DB_DEPENDENCIES: &str = "db.dependencies";
    /// Batched `IN (...)` query for the "AI reviewing" badge.
    pub const DB_AI_REVIEWING: &str = "db.ai_reviewing";
    /// Batched `IN (...)` query for the resolved AI-review-state badge.
    pub const DB_AI_REVIEW_STATE: &str = "db.ai_review_state";
    /// Family-key read that probes the review cache (`read_family_keys`).
    pub const DB_AI_REVIEW_STATE_FAMILY_KEYS: &str = "db.ai_review_state.family_keys";
    /// Current-head verdict query (`fetch_current_heads`).
    pub const DB_AI_REVIEW_STATE_CURRENT_HEADS: &str = "db.ai_review_state.current_heads";
    /// Badge-history query (`fetch_badge_histories`).
    pub const DB_AI_REVIEW_STATE_BADGE_HISTORIES: &str = "db.ai_review_state.badge_histories";
    /// Batched `IN (...)` query for the screenshot-viewer card affordance.
    pub const DB_HAS_ATTACHMENTS: &str = "db.has_attachments";
    /// Batched `IN (...)` query for the PR review-guide card affordance.
    pub const DB_REVIEW_GUIDE_STATE: &str = "db.review_guide_state";
    /// Per-task doc-pointer resolution loop — a secondary, gated N+1.
    pub const DB_DOC_POINTERS: &str = "db.doc_pointers";
    /// In-memory projection / flag attachment (no DB).
    pub const ASSEMBLE: &str = "assemble";
    /// Time the handler spent blocked on the shared board load (its own
    /// build, or waiting on the request that owns it).
    pub const SHARED_LOAD_WAIT: &str = "shared_load_wait";
    /// Handler enqueue → writer-task dequeue. Non-zero only when the
    /// session's writer queue has other events ahead of this response;
    /// otherwise the gap is negligible and this segment stays near zero.
    pub const QUEUE_WAIT: &str = "queue_wait";
    /// `serde_json` serialization of the whole `WorkTree` response.
    pub const SERIALIZE: &str = "serialize";
    /// Bytes written + flushed to the socket.
    pub const SOCKET_WRITE: &str = "socket_write";
    /// Whole engine window: request line receipt → last byte flushed.
    pub const TOTAL: &str = "total";
}

/// One timing line. Optional fields are omitted from the JSON when `None`
/// (serde `skip_serializing_if`), so a `decode` line stays lean while a
/// `db.task_runtimes` line carries the full N+1 breakdown.
#[derive(Debug, Clone, Serialize, bon::Builder)]
#[builder(on(String, into))]
pub struct EnginePopulationRecord {
    /// Wall-clock time the segment was recorded, ms since the Unix epoch.
    pub ts_epoch_ms: u128,
    pub product_id: String,
    /// Engine-side envelope request id — unique per fetch, groups this
    /// trace's segments together.
    pub request_id: String,
    /// App-side per-product fetch sequence, when the app sent one. The
    /// join key with `population-timing-*.jsonl`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_seq: Option<i64>,
    /// Segment name (see [`segment`]).
    pub segment: &'static str,
    pub duration_ms: f64,
    /// Rows returned by the segment's query, or items iterated by an N+1
    /// loop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<i64>,
    /// SQL statements executed in the segment. Present on N+1 stages so
    /// `db_queries / rows` reveals the per-row subquery fan-out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_queries: Option<i64>,
    /// Serialized response size in bytes (`serialize` segment only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_bytes: Option<i64>,
    /// Total items (tasks + chores) in this fetch, carried on aggregate
    /// segments so cost correlates with cardinality (parity with the
    /// app-side `items` field).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<i64>,
    /// Review-cache hits for `db.ai_review_state` (owners served from cache).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hits: Option<i64>,
    /// Review-cache misses for `db.ai_review_state` (owners recomputed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_misses: Option<i64>,
    /// Distinct PR owners resolved in `db.ai_review_state`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owners: Option<i64>,
}

impl boss_engine_day_rotated_log::TimestampedRecord for EnginePopulationRecord {
    fn ts_epoch_ms(&self) -> u128 {
        self.ts_epoch_ms
    }
}

/// One recorded segment, pending flush. Stamped with its own wall-clock
/// time so the flushed lines preserve per-segment ordering even though a
/// whole trace is emitted at once.
#[derive(Debug, Clone, bon::Builder)]
struct PendingSegment {
    ts_epoch_ms: u128,
    segment: &'static str,
    duration_ms: f64,
    rows: Option<i64>,
    db_queries: Option<i64>,
    payload_bytes: Option<i64>,
    cache_hits: Option<i64>,
    cache_misses: Option<i64>,
    owners: Option<i64>,
}

/// Per-fetch correlation shared by every segment of one trace: the ids and
/// item count the emitted lines carry so a trace's segments group together
/// and join to the app side. Kept as a sub-struct so [`PopulationTrace`]
/// stays under the giant-structs field threshold.
#[derive(Debug, Clone)]
struct TraceContext {
    product_id: String,
    request_id: String,
    fetch_seq: Option<i64>,
    items: Option<i64>,
}

/// Accumulator for one `GetWorkTree` fetch. Built at the reader/handler
/// boundary, filled as the request flows through decode → DB → assemble,
/// then handed to the writer task (via the per-session registry) which
/// appends the `serialize` / `socket_write` / `total` segments and flushes.
///
/// A `disabled` trace (`started == None`) records nothing and is never
/// flushed — it lets the uninstrumented `WorkDb::get_work_tree` (called from
/// tests and other paths) share the same body without emitting. Enabled ⇔
/// `started.is_some()`.
#[derive(Debug)]
pub struct PopulationTrace {
    ctx: TraceContext,
    /// Engine window start: when the request line was received, before
    /// decode. `None` marks a disabled trace.
    started: Option<Instant>,
    segments: Vec<PendingSegment>,
    /// Set by [`Self::mark_enqueued`] when the handler hands the trace to
    /// the writer task's session queue. [`Self::record_queue_wait`] turns
    /// the gap between that instant and its own call into the `queue_wait`
    /// segment, then clears this so a repeat call is a no-op.
    enqueued_at: Option<Instant>,
    /// Live cold-build progress; inert unless a sink was installed with
    /// [`Self::set_progress_sink`].
    progress: ProgressReporter,
}

impl PopulationTrace {
    /// An enabled trace whose window starts at `started` (request line
    /// receipt). `fetch_seq` is the app's correlation id, if it sent one.
    pub fn new(
        product_id: impl Into<String>,
        request_id: impl Into<String>,
        fetch_seq: Option<i64>,
        started: Instant,
    ) -> Self {
        Self {
            ctx: TraceContext {
                product_id: product_id.into(),
                request_id: request_id.into(),
                fetch_seq,
                items: None,
            },
            started: Some(started),
            segments: Vec::new(),
            enqueued_at: None,
            progress: ProgressReporter::default(),
        }
    }

    /// A no-op trace: the DB body fills it, but nothing is retained or
    /// emitted. Used by the uninstrumented `get_work_tree` wrapper.
    pub fn disabled() -> Self {
        Self {
            ctx: TraceContext {
                product_id: String::new(),
                request_id: String::new(),
                fetch_seq: None,
                items: None,
            },
            started: None,
            segments: Vec::new(),
            enqueued_at: None,
            progress: ProgressReporter::default(),
        }
    }

    /// Whether this trace will emit. Lets hot callers skip building strings
    /// they only need when instrumented.
    pub fn is_enabled(&self) -> bool {
        self.started.is_some()
    }

    /// Announce a stage before beginning its work, separately from duration logging.
    pub fn enter_stage(&self, name: &'static str) {
        self.progress.stage(name, self.elapsed_ms(), self.ctx.items);
    }

    /// Writer stages go directly to the socket ahead of the snapshot.
    pub fn writer_stage(&self, name: &'static str) -> Option<(&str, crate::population_progress::PopulationProgress)> {
        self.progress
            .stage_report(name, self.elapsed_ms(), self.ctx.items)
            .map(|report| (self.ctx.product_id.as_str(), report))
    }

    /// Record a plain timed segment (duration only).
    pub fn record_plain(&mut self, segment: &'static str, duration_ms: f64) {
        self.push(segment, duration_ms, None, None, None);
    }

    /// Record a single-query segment carrying its row count.
    pub fn record_query(&mut self, segment: &'static str, duration_ms: f64, rows: usize) {
        self.push(segment, duration_ms, Some(rows as i64), None, None);
    }

    /// Record an N+1 segment: `rows` items iterated across `db_queries`
    /// SQL statements.
    pub fn record_nplus1(&mut self, segment: &'static str, duration_ms: f64, rows: usize, db_queries: u64) {
        self.push(segment, duration_ms, Some(rows as i64), Some(db_queries as i64), None);
    }

    /// Record the AI-review-state attach: cards, statements, cache traffic,
    /// and the owner cardinality of the lookup.
    pub fn record_ai_review_state(
        &mut self,
        duration_ms: f64,
        cards: usize,
        db_queries: u64,
        cache_hits: u64,
        cache_misses: u64,
        owners: usize,
    ) {
        if !self.is_enabled() {
            return;
        }
        self.segments.push(
            PendingSegment::builder()
                .ts_epoch_ms(boss_engine_day_rotated_log::now_ms())
                .segment(segment::DB_AI_REVIEW_STATE)
                .duration_ms(duration_ms)
                .rows(cards as i64)
                .db_queries(db_queries as i64)
                .cache_hits(cache_hits as i64)
                .cache_misses(cache_misses as i64)
                .owners(owners as i64)
                .build(),
        );
    }

    /// Record the serialize segment, carrying the payload size.
    pub fn record_serialize(&mut self, duration_ms: f64, payload_bytes: usize) {
        self.push(segment::SERIALIZE, duration_ms, None, None, Some(payload_bytes as i64));
    }

    /// Set the total item count (tasks + chores) once the DB body knows it.
    pub fn set_items(&mut self, items: usize) {
        if self.is_enabled() {
            self.ctx.items = Some(items as i64);
        }
    }

    /// Report this trace's cold-build progress to `sink`. No-op on a
    /// disabled trace, which records nothing to report.
    pub fn set_progress_sink(&mut self, sink: ProgressSink) {
        if self.is_enabled() {
            self.progress.set_sink(sink);
        }
    }

    /// Mark the start of a full board read (as opposed to an incremental
    /// refresh) so progress is reported for it. Called by the board
    /// projection immediately before it assembles the whole tree.
    pub fn begin_cold_build(&mut self) {
        if self.is_enabled() {
            self.progress.start(self.elapsed_ms(), self.ctx.items);
        }
    }

    /// Close a reported cold build with the snapshot's item count and
    /// serialized size. The writer calls this once the frame is written; a
    /// no-op unless [`Self::begin_cold_build`] reported a start.
    pub fn finish_progress(&mut self) {
        if self.is_enabled() {
            let bytes = self.payload_bytes();
            self.progress.finish(self.elapsed_ms(), self.ctx.items, bytes);
        }
    }

    /// The breakdown of this population if its `total` exceeded
    /// [`SLOW_POPULATION_MS`], else `None`. Needs the `total` segment, so the
    /// writer asks after recording it.
    pub fn slow_population(&self) -> Option<SlowPopulation> {
        let total_ms = self.segments.iter().find(|s| s.segment == segment::TOTAL)?.duration_ms;
        (total_ms > SLOW_POPULATION_MS).then(|| SlowPopulation {
            total_ms,
            items: self.ctx.items,
            bytes: self.payload_bytes(),
            stages: self
                .segments
                .iter()
                .filter(|s| s.segment != segment::TOTAL)
                .map(|s| (s.segment, s.duration_ms))
                .collect(),
        })
    }

    /// Log a slow population's stages at INFO with the product id and the
    /// snapshot's item and byte counts. No-op when the population was fast.
    pub fn log_if_slow(&self) {
        if let Some(slow) = self.slow_population() {
            tracing::info!(
                product_id = %self.ctx.product_id,
                request_id = %self.ctx.request_id,
                total_ms = slow.total_ms,
                items = ?slow.items,
                bytes = ?slow.bytes,
                stages = %slow.stages_summary(),
                "slow board population"
            );
        }
    }

    fn payload_bytes(&self) -> Option<i64> {
        self.segments.iter().find_map(|s| s.payload_bytes)
    }

    /// Elapsed ms since the window start, or `0.0` if disabled/unset.
    /// Used by the writer to compute the `total` segment.
    pub fn elapsed_ms(&self) -> f64 {
        self.started.map(elapsed_ms).unwrap_or(0.0)
    }

    /// Mark the instant the handler hands this trace off to the writer
    /// task's session queue (stashed just before the response is enqueued).
    /// No-op on a disabled trace.
    pub fn mark_enqueued(&mut self) {
        if self.is_enabled() {
            self.enqueued_at = Some(Instant::now());
        }
    }

    /// Record the `queue_wait` segment: the gap between [`Self::mark_enqueued`]
    /// and this call, i.e. how long the response sat in the writer task's
    /// session queue before being dequeued for serialization. No-op if
    /// `mark_enqueued` was never called (or the trace is disabled).
    pub fn record_queue_wait(&mut self) {
        if let Some(enqueued_at) = self.enqueued_at.take() {
            self.record_plain(segment::QUEUE_WAIT, elapsed_ms(enqueued_at));
        }
    }

    fn push(
        &mut self,
        segment: &'static str,
        duration_ms: f64,
        rows: Option<i64>,
        db_queries: Option<i64>,
        payload_bytes: Option<i64>,
    ) {
        if !self.is_enabled() {
            return;
        }
        self.segments.push(
            PendingSegment::builder()
                .ts_epoch_ms(boss_engine_day_rotated_log::now_ms())
                .segment(segment)
                .duration_ms(duration_ms)
                .maybe_rows(rows)
                .maybe_db_queries(db_queries)
                .maybe_payload_bytes(payload_bytes)
                .build(),
        );
    }

    /// Emit every recorded segment to `log`. No-op for a disabled trace.
    pub fn flush(&self, log: &PopulationTimingLog) {
        if !self.is_enabled() {
            return;
        }
        for seg in &self.segments {
            log.emit(
                EnginePopulationRecord::builder()
                    .ts_epoch_ms(seg.ts_epoch_ms)
                    .product_id(self.ctx.product_id.as_str())
                    .request_id(self.ctx.request_id.as_str())
                    .maybe_fetch_seq(self.ctx.fetch_seq)
                    .segment(seg.segment)
                    .duration_ms(seg.duration_ms)
                    .maybe_rows(seg.rows)
                    .maybe_db_queries(seg.db_queries)
                    .maybe_payload_bytes(seg.payload_bytes)
                    .maybe_cache_hits(seg.cache_hits)
                    .maybe_cache_misses(seg.cache_misses)
                    .maybe_owners(seg.owners)
                    .maybe_items(self.ctx.items)
                    .build(),
            );
        }
    }
}

impl PopulationTrace {
    /// `(duration_ms, rows, db_queries)` for the first recorded segment named
    /// `name`. Storage tests assert the N+1 fan-out captured by
    /// `get_work_tree_instrumented` without a disk round-trip.
    #[cfg(any(test, feature = "test-support"))]
    pub fn segment_counts(&self, name: &str) -> Option<(f64, Option<i64>, Option<i64>)> {
        self.segments
            .iter()
            .find(|s| s.segment == name)
            .map(|s| (s.duration_ms, s.rows, s.db_queries))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn segment_review_counts(&self, name: &str) -> Option<(Option<i64>, Option<i64>, Option<i64>)> {
        self.segments
            .iter()
            .find(|s| s.segment == name)
            .map(|s| (s.cache_hits, s.cache_misses, s.owners))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn request_id(&self) -> &str {
        &self.ctx.request_id
    }
}

/// Milliseconds elapsed since `start`, as an `f64` with sub-ms resolution.
pub fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

const FILE_PREFIX: &str = "engine-population-timing-";

/// Non-blocking, append-only day-rotated writer for the engine
/// population-timing log. Built on [`boss_engine_day_rotated_log::DayRotatedLogger`],
/// the same machinery [`crate::ipc_log::IpcLogger`] uses.
pub struct PopulationTimingLog {
    inner: DayRotatedLogger<EnginePopulationRecord>,
}

impl PopulationTimingLog {
    /// Create a logger that writes day files directly under `dir`. Spawns
    /// the background writer task when a Tokio runtime is available; when
    /// created outside a runtime (synchronous tests) entries queue and are
    /// dropped on sender drop.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: DayRotatedLogger::new(dir, FILE_PREFIX),
        }
    }

    /// Fire-and-forget a segment record. Dropped silently if the writer
    /// task has exited.
    pub fn emit(&self, rec: EnginePopulationRecord) {
        self.inner.emit(rec);
    }
}

static LOG: OnceLock<Option<PopulationTimingLog>> = OnceLock::new();

/// Install the process-wide logger, writing under `dir` (`None` disables it).
///
/// Called once at engine startup with the directory the isolation guard
/// resolved and gated (`IsolationPaths::resolve_diagnostics_dir`). Nothing
/// here reads the environment or `$HOME`, so a fixture uses only its checked
/// diagnostics directory. Returns
/// `false`, leaving the first installation in place, when already installed.
pub fn install_global(dir: Option<PathBuf>) -> bool {
    LOG.set(dir.map(PopulationTimingLog::new)).is_ok()
}

/// The process-wide logger. `None` until [`install_global`] runs, and after it
/// when no diagnostics directory could be determined — in which case
/// instrumentation silently does nothing.
pub fn global() -> Option<&'static PopulationTimingLog> {
    LOG.get()?.as_ref()
}

/// The `BOSS_ENGINE_DIAGNOSTICS_DIR` override, when set to a non-empty
/// (trimmed) value.
pub fn diagnostics_dir_override() -> Option<PathBuf> {
    parse_diagnostics_dir_override(std::env::var_os(DIAGNOSTICS_DIR_ENV))
}

fn parse_diagnostics_dir_override(raw: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let trimmed = raw?.to_string_lossy().trim().to_owned();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_trace_records_nothing() {
        let mut trace = PopulationTrace::disabled();
        assert!(!trace.is_enabled());
        trace.record_plain(segment::DECODE, 1.0);
        trace.record_query(segment::DB_TASKS, 2.0, 100);
        trace.record_nplus1(segment::DB_TASK_RUNTIMES, 3.0, 100, 250);
        trace.set_items(100);
        assert!(trace.segments.is_empty());
        assert_eq!(trace.elapsed_ms(), 0.0);
    }

    #[test]
    fn enabled_trace_accumulates_segments() {
        let mut trace = PopulationTrace::new("prod-1", "req-1", Some(7), Instant::now());
        trace.record_plain(segment::DECODE, 0.5);
        trace.record_query(segment::DB_TASKS, 12.0, 1949);
        trace.record_nplus1(segment::DB_TASK_RUNTIMES, 6000.0, 1949, 4200);
        trace.record_serialize(80.0, 2_500_000);
        trace.set_items(1949);

        assert_eq!(trace.segments.len(), 4);
        let runtimes = trace
            .segments
            .iter()
            .find(|s| s.segment == segment::DB_TASK_RUNTIMES)
            .unwrap();
        assert_eq!(runtimes.rows, Some(1949));
        assert_eq!(runtimes.db_queries, Some(4200));
        let serialize = trace.segments.iter().find(|s| s.segment == segment::SERIALIZE).unwrap();
        assert_eq!(serialize.payload_bytes, Some(2_500_000));
    }

    /// Every report a trace sends to its sink, in order.
    fn collecting_sink() -> (
        ProgressSink,
        std::sync::Arc<std::sync::Mutex<Vec<crate::population_progress::PopulationProgress>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let sink: ProgressSink = std::sync::Arc::new(move |p| sink_seen.lock().unwrap().push(p));
        (sink, seen)
    }

    #[test]
    fn cold_build_reports_start_live_stages_and_done_with_counts() {
        use boss_protocol::BoardPopulationPhase::{Done, Stage, Started};
        let (sink, seen) = collecting_sink();
        let mut trace = PopulationTrace::new("prod-1", "req-1", None, Instant::now());
        trace.set_progress_sink(sink);
        trace.record_plain(segment::DECODE, 0.5);
        trace.begin_cold_build();
        trace.enter_stage(segment::DB_TASKS);
        trace.record_query(segment::DB_TASKS, 12.0, 1949);
        trace.set_items(1949);
        trace.enter_stage(segment::DB_TASK_RUNTIMES);
        trace.record_nplus1(segment::DB_TASK_RUNTIMES, 6000.0, 1949, 4200);
        trace.enter_stage(segment::ASSEMBLE);
        trace.record_plain(segment::ASSEMBLE, 3.0);
        trace.record_plain(segment::SHARED_LOAD_WAIT, 6015.0);
        trace.record_serialize(80.0, 31_500_000);
        trace.record_plain(segment::SOCKET_WRITE, 9.0);
        trace.record_plain(segment::TOTAL, 6200.0);
        trace.finish_progress();

        let seen = seen.lock().unwrap();
        let shape: Vec<_> = seen.iter().map(|p| (p.phase, p.stage)).collect();
        assert_eq!(
            shape,
            vec![
                (Started, None),
                (Stage, Some(segment::DB_TASKS)),
                (Stage, Some(segment::DB_TASK_RUNTIMES)),
                (Stage, Some(segment::ASSEMBLE)),
                (Done, None),
            ],
            "only explicit stage entries publish progress; writer stages use direct frames"
        );
        assert_eq!(
            seen[2].items,
            Some(1949),
            "stage events carry the item count once known"
        );
        let done = seen.last().unwrap();
        assert_eq!((done.items, done.bytes), (Some(1949), Some(31_500_000)));
    }

    #[test]
    fn warm_refresh_without_begin_cold_build_reports_nothing() {
        let (sink, seen) = collecting_sink();
        let mut trace = PopulationTrace::new("prod-1", "req-1", None, Instant::now());
        trace.set_progress_sink(sink);
        trace.record_query(segment::DB_TASKS, 1.0, 10);
        trace.record_plain(segment::TOTAL, 2.0);
        trace.finish_progress();
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn begin_cold_build_reports_a_single_start() {
        let (sink, seen) = collecting_sink();
        let mut trace = PopulationTrace::new("prod-1", "req-1", None, Instant::now());
        trace.set_progress_sink(sink);
        trace.begin_cold_build();
        trace.begin_cold_build();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn disabled_trace_ignores_a_progress_sink() {
        let (sink, seen) = collecting_sink();
        let mut trace = PopulationTrace::disabled();
        trace.set_progress_sink(sink);
        trace.begin_cold_build();
        trace.record_query(segment::DB_TASKS, 1.0, 10);
        trace.finish_progress();
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn slow_population_needs_total_over_the_threshold() {
        let mut trace = PopulationTrace::new("prod-1", "req-1", None, Instant::now());
        trace.record_query(segment::DB_TASKS, 100.0, 3);
        assert_eq!(trace.slow_population(), None, "no total segment yet");
        trace.record_plain(segment::TOTAL, SLOW_POPULATION_MS);
        assert_eq!(trace.slow_population(), None, "exactly at the threshold is not slow");

        let mut trace = PopulationTrace::new("prod-1", "req-1", None, Instant::now());
        trace.record_query(segment::DB_TASKS, 100.0, 1949);
        trace.record_nplus1(segment::DB_TASK_RUNTIMES, 113_000.0, 1949, 4200);
        trace.set_items(1949);
        trace.record_serialize(900.0, 31_500_000);
        trace.record_plain(segment::TOTAL, 114_500.0);
        let slow = trace.slow_population().expect("114s is slow");
        assert_eq!(slow.total_ms, 114_500.0);
        assert_eq!((slow.items, slow.bytes), (Some(1949), Some(31_500_000)));
        assert_eq!(
            slow.stages_summary(),
            "db.tasks=100ms db.task_runtimes=113000ms serialize=900ms",
            "stages in recorded order, total excluded"
        );
    }

    #[test]
    fn queue_wait_records_one_segment_after_enqueue() {
        let mut trace = PopulationTrace::new("prod-1", "req-1", None, Instant::now());
        trace.mark_enqueued();
        trace.record_queue_wait();

        let queue_waits: Vec<_> = trace
            .segments
            .iter()
            .filter(|s| s.segment == segment::QUEUE_WAIT)
            .collect();
        assert_eq!(queue_waits.len(), 1, "exactly one queue_wait segment");
        assert_eq!(queue_waits[0].segment, "queue_wait");
    }

    #[test]
    fn queue_wait_is_noop_without_mark_enqueued() {
        let mut trace = PopulationTrace::new("prod-1", "req-1", None, Instant::now());
        // No mark_enqueued() call — the documented no-op contract.
        trace.record_queue_wait();
        assert!(
            !trace.segments.iter().any(|s| s.segment == segment::QUEUE_WAIT),
            "record_queue_wait must not push a segment when mark_enqueued was never called"
        );
        assert!(trace.segments.is_empty());
    }

    #[test]
    fn disabled_trace_records_no_queue_wait() {
        let mut trace = PopulationTrace::disabled();
        trace.mark_enqueued();
        trace.record_queue_wait();
        assert!(!trace.is_enabled());
        assert!(trace.segments.is_empty(), "disabled trace records nothing");
    }

    #[test]
    fn queue_wait_fires_only_once_per_mark_enqueued() {
        let mut trace = PopulationTrace::new("prod-1", "req-1", None, Instant::now());
        trace.mark_enqueued();
        trace.record_queue_wait();
        // Second call without re-marking: enqueued_at was taken, so this is a no-op.
        trace.record_queue_wait();

        let count = trace
            .segments
            .iter()
            .filter(|s| s.segment == segment::QUEUE_WAIT)
            .count();
        assert_eq!(count, 1, "queue_wait must record once per mark_enqueued");
    }

    #[tokio::test]
    async fn flush_propagates_items_and_per_segment_fields() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = PopulationTimingLog::new(dir.path());

        let mut trace = PopulationTrace::new("prod-9", "req-9", Some(42), Instant::now());
        trace.record_plain(segment::DECODE, 0.5);
        trace.record_query(segment::DB_TASKS, 12.0, 1949);
        trace.record_nplus1(segment::DB_TASK_RUNTIMES, 6000.0, 1949, 4200);
        trace.record_serialize(80.0, 2_500_000);
        trace.set_items(1949);
        trace.flush(&log);

        // Let the background writer task drain the channel to disk.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 1, "one daily log file");
        let content = std::fs::read_to_string(dir.path().join(&files[0])).unwrap();
        let records: Vec<serde_json::Value> = content.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(records.len(), 4, "one emitted record per recorded segment");

        // Trace-level correlation + items ride on every emitted record.
        for rec in &records {
            assert_eq!(rec["product_id"], "prod-9");
            assert_eq!(rec["request_id"], "req-9");
            assert_eq!(rec["fetch_seq"], 42);
            assert_eq!(rec["items"], 1949, "trace-level items on every segment");
        }

        let by_segment = |name: &str| records.iter().find(|r| r["segment"] == name).unwrap();

        // Plain segment carries neither rows nor payload bytes.
        let decode = by_segment("decode");
        assert!(decode.get("rows").is_none());
        assert!(decode.get("payload_bytes").is_none());

        // Single-query segment carries its row count.
        assert_eq!(by_segment("db.tasks")["rows"], 1949);

        // N+1 segment carries rows and the SQL statement fan-out.
        let runtimes = by_segment("db.task_runtimes");
        assert_eq!(runtimes["rows"], 1949);
        assert_eq!(runtimes["db_queries"], 4200);

        // Serialize segment carries the payload size.
        assert_eq!(by_segment("serialize")["payload_bytes"], 2_500_000);
    }

    #[test]
    fn record_serializes_to_expected_json_shape() {
        let rec = EnginePopulationRecord {
            ts_epoch_ms: 1_778_716_800_000,
            product_id: "prod-1".to_owned(),
            request_id: "req-1".to_owned(),
            fetch_seq: Some(7),
            segment: segment::DB_TASK_RUNTIMES,
            duration_ms: 6000.0,
            rows: Some(1949),
            db_queries: Some(4200),
            payload_bytes: None,
            items: Some(1949),
            cache_hits: None,
            cache_misses: None,
            owners: None,
        };
        let json: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["segment"], "db.task_runtimes");
        assert_eq!(json["rows"], 1949);
        assert_eq!(json["db_queries"], 4200);
        assert_eq!(json["fetch_seq"], 7);
        // Omitted-when-None fields must not appear.
        assert!(json.get("payload_bytes").is_none());
    }

    #[test]
    fn record_omits_absent_correlation_and_counts() {
        let rec = EnginePopulationRecord {
            ts_epoch_ms: 1_778_716_800_000,
            product_id: "prod-1".to_owned(),
            request_id: "req-1".to_owned(),
            fetch_seq: None,
            segment: segment::DECODE,
            duration_ms: 0.4,
            rows: None,
            db_queries: None,
            payload_bytes: None,
            items: None,
            cache_hits: None,
            cache_misses: None,
            owners: None,
        };
        let json: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert!(json.get("fetch_seq").is_none());
        assert!(json.get("rows").is_none());
        assert!(json.get("db_queries").is_none());
        assert!(json.get("items").is_none());
        assert_eq!(json["request_id"], "req-1");
    }

    #[test]
    fn ai_review_state_record_includes_cache_and_owner_counts() {
        let mut trace = PopulationTrace::new("prod-1", "req-1", None, Instant::now());
        trace.record_ai_review_state(12.5, 80, 4, 10, 20, 30);
        assert_eq!(
            trace.segment_counts(segment::DB_AI_REVIEW_STATE),
            Some((12.5, Some(80), Some(4)))
        );
        assert_eq!(
            trace.segment_review_counts(segment::DB_AI_REVIEW_STATE),
            Some((Some(10), Some(20), Some(30)))
        );
        let rec = EnginePopulationRecord {
            ts_epoch_ms: 1,
            product_id: "prod-1".to_owned(),
            request_id: "req-1".to_owned(),
            fetch_seq: None,
            segment: segment::DB_AI_REVIEW_STATE,
            duration_ms: 12.5,
            rows: Some(80),
            db_queries: Some(4),
            payload_bytes: None,
            items: None,
            cache_hits: Some(10),
            cache_misses: Some(20),
            owners: Some(30),
        };
        let json: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["cache_hits"], 10);
        assert_eq!(json["cache_misses"], 20);
        assert_eq!(json["owners"], 30);
    }

    #[tokio::test]
    async fn writer_task_writes_and_rotates_by_day() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = PopulationTimingLog::new(dir.path());

        log.emit(EnginePopulationRecord {
            ts_epoch_ms: boss_engine_day_rotated_log::now_ms(),
            product_id: "prod-1".to_owned(),
            request_id: "req-1".to_owned(),
            fetch_seq: Some(3),
            segment: segment::TOTAL,
            duration_ms: 7100.0,
            rows: None,
            db_queries: None,
            payload_bytes: None,
            items: Some(1949),
            cache_hits: None,
            cache_misses: None,
            owners: None,
        });

        // Let the background task flush.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 1, "one daily log file");
        assert!(files[0].starts_with("engine-population-timing-"));
        assert!(files[0].ends_with(".jsonl"));

        let content = std::fs::read_to_string(dir.path().join(&files[0])).unwrap();
        let line = content.lines().next().unwrap();
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(entry["segment"], "total");
        assert_eq!(entry["product_id"], "prod-1");
        assert_eq!(entry["fetch_seq"], 3);
        assert_eq!(entry["items"], 1949);
    }

    #[test]
    fn diagnostics_dir_override_is_trimmed_and_blank_is_ignored() {
        let parse = |raw: Option<&str>| parse_diagnostics_dir_override(raw.map(Into::into));
        assert_eq!(
            parse(Some("  /tmp/boss-diag-test  ")),
            Some(PathBuf::from("/tmp/boss-diag-test"))
        );
        assert_eq!(parse(Some("   ")), None);
        assert_eq!(parse(Some("")), None);
        assert_eq!(parse(None), None);
    }
}
