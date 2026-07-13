//! Always-on, cheap engine-side wall-clock instrumentation for the
//! `GetWorkTree` task-population RPC — the one request that repopulates
//! the macOS kanban on cold start and product switch.
//!
//! # Motivation
//!
//! The app-side diagnostics from investigation T2101 (PR #1697) proved the
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
//!   db.doc_pointers  per-task doc-pointer resolution (gated N+1)
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
//! for local runs / tests via `BOSS_ENGINE_DIAGNOSTICS_DIR`.) Rotation,
//! retention, and the background-writer task are the generic machinery in
//! [`crate::day_rotated_log`], shared with [`crate::ipc_log`].
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
use std::time::Instant;

use serde::Serialize;

use crate::day_rotated_log::DayRotatedLogger;

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
    /// Per-task doc-pointer resolution loop — a secondary, gated N+1.
    pub const DB_DOC_POINTERS: &str = "db.doc_pointers";
    /// In-memory projection / flag attachment (no DB).
    pub const ASSEMBLE: &str = "assemble";
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
}

impl crate::day_rotated_log::TimestampedRecord for EnginePopulationRecord {
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
        }
    }

    /// Whether this trace will emit. Lets hot callers skip building strings
    /// they only need when instrumented.
    pub fn is_enabled(&self) -> bool {
        self.started.is_some()
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
                .ts_epoch_ms(crate::day_rotated_log::now_ms())
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
                    .maybe_items(self.ctx.items)
                    .build(),
            );
        }
    }
}

#[cfg(test)]
impl PopulationTrace {
    /// Test-only: `(duration_ms, rows, db_queries)` for the first recorded
    /// segment named `name`. Lets in-crate tests assert the N+1 fan-out
    /// captured by `get_work_tree_instrumented` without a disk round-trip.
    pub(crate) fn segment_counts(&self, name: &str) -> Option<(f64, Option<i64>, Option<i64>)> {
        self.segments
            .iter()
            .find(|s| s.segment == name)
            .map(|s| (s.duration_ms, s.rows, s.db_queries))
    }
}

/// Milliseconds elapsed since `start`, as an `f64` with sub-ms resolution.
pub fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

const FILE_PREFIX: &str = "engine-population-timing-";

/// Non-blocking, append-only day-rotated writer for the engine
/// population-timing log. Built on [`crate::day_rotated_log::DayRotatedLogger`],
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

/// The process-wide logger, resolved once. `None` when no diagnostics
/// directory can be determined (`HOME` unset and no env override) — in
/// which case instrumentation silently does nothing.
pub fn global() -> Option<&'static PopulationTimingLog> {
    use std::sync::OnceLock;
    static LOG: OnceLock<Option<PopulationTimingLog>> = OnceLock::new();
    LOG.get_or_init(|| resolve_diagnostics_dir().map(PopulationTimingLog::new))
        .as_ref()
}

/// `<boss-state-root>/diagnostics`, or the `BOSS_ENGINE_DIAGNOSTICS_DIR`
/// override when set to a non-empty (trimmed) value.
fn resolve_diagnostics_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(DIAGNOSTICS_DIR_ENV) {
        let trimmed = raw.to_string_lossy().trim().to_owned();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    Some(boss_log_files::default_state_root()?.join("diagnostics"))
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
        };
        let json: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert!(json.get("fetch_seq").is_none());
        assert!(json.get("rows").is_none());
        assert!(json.get("db_queries").is_none());
        assert!(json.get("items").is_none());
        assert_eq!(json["request_id"], "req-1");
    }

    #[tokio::test]
    async fn writer_task_writes_and_rotates_by_day() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = PopulationTimingLog::new(dir.path());

        log.emit(EnginePopulationRecord {
            ts_epoch_ms: crate::day_rotated_log::now_ms(),
            product_id: "prod-1".to_owned(),
            request_id: "req-1".to_owned(),
            fetch_seq: Some(3),
            segment: segment::TOTAL,
            duration_ms: 7100.0,
            rows: None,
            db_queries: None,
            payload_bytes: None,
            items: Some(1949),
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
    fn diagnostics_dir_env_override_wins() {
        // Serialize env mutation is unnecessary: this is the only test that
        // reads this var, and it removes it afterwards.
        unsafe {
            std::env::set_var(DIAGNOSTICS_DIR_ENV, "  /tmp/boss-diag-test  ");
        }
        assert_eq!(resolve_diagnostics_dir(), Some(PathBuf::from("/tmp/boss-diag-test")));
        unsafe {
            std::env::set_var(DIAGNOSTICS_DIR_ENV, "   ");
        }
        // Empty override is ignored → falls back to state root (present in CI).
        let fallback = resolve_diagnostics_dir();
        assert_ne!(fallback, Some(PathBuf::from("")));
        unsafe {
            std::env::remove_var(DIAGNOSTICS_DIR_ENV);
        }
    }
}
