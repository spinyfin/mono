//! Escalates a persistent dispatch stage-stall into a durable,
//! operator-visible attention item.
//!
//! [`crate::dispatch_reader::pending_stalls`] already emits a
//! `stage_stalled` dispatch event once a stage sits past its per-stage
//! threshold (~30-120s, see `app/server.rs`'s `StageThresholds`) — but
//! that event is write-only telemetry: no attention item, no alert,
//! pull-only via `bossctl dispatch ghost-active --include-stalled`
//! (`dispatch_events.rs`'s own doc on `Stage::StageStalled` calls out
//! that it "does NOT auto-remediate"). A stage stuck for minutes, not
//! seconds, is a materially different, higher-severity condition an
//! operator should see on the kanban/attention surface without knowing
//! to run that verb.
//!
//! This sweep re-scans the same per-execution `dispatch.jsonl` mirrors
//! [`crate::dispatch_reader::persistently_stalled`] reads, on a slower
//! cadence and a much larger flat threshold, and files a
//! `dispatch_stage_stalled` attention item
//! (`crate::work::DISPATCH_STAGE_STALLED_ATTENTION_KIND`) for each one —
//! idempotently, via `WorkDb::file_dispatch_stage_stalled_attention`, so
//! repeated ticks refresh the same row instead of piling up duplicates.
//! The item resolves when the execution finally claims a worker slot
//! (`Coordinator::dispatch_claimed_execution`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::dispatch_reader;
use crate::work::{StallEscalation, WorkDb};

/// How long a dispatch stage must be stuck before it graduates from
/// write-only `stage_stalled` telemetry to an operator-visible attention
/// item. Deliberately much larger than any individual per-stage
/// `StageThresholds` entry (30-120s) — those exist to catch a stage
/// regression quickly in logs/JSONL; this exists so a human doesn't have
/// to notice a multi-minute stall on their own.
pub const PERSISTENT_STALL_THRESHOLD: Duration = Duration::from_secs(5 * 60);

/// Default cadence for [`spawn_loop`]. Slower than the 15s
/// `stage_stalled` detector — this sweep only matters once a stall has
/// already run for [`PERSISTENT_STALL_THRESHOLD`], so there is no value
/// in polling faster than that.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Spawn a tokio task that runs [`run_one_pass`] every `interval`, filing
/// a `dispatch_stage_stalled` attention item for any execution whose
/// dispatch timeline has been stuck past `threshold`.
pub fn spawn_loop(
    root: PathBuf,
    work_db: Arc<WorkDb>,
    threshold: Duration,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(interval).await;
        loop {
            match run_one_pass(&root, work_db.as_ref(), threshold) {
                Ok(outcome) => log_pass(&outcome),
                Err(err) => tracing::warn!(?err, "dispatch stall attention sweep: pass failed"),
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Emit **exactly one** aggregate line per pass. A per-item log inside a
/// 60s sweep is the footgun this module was filed to fix: a backlog of
/// dead/automation executions (whose `dispatch.jsonl` mirrors linger on
/// disk long after the execution is gone) turned it into ~3k WARN lines
/// per pass — ~90% of all engine-trace volume, rotating a 100 MB trace
/// every ~44 minutes and tearing it with partial JSON writes right when an
/// operator needs it during an incident.
///
/// A genuine filing error (rare once the liveness/id filters run) is worth
/// a single WARN; a normal filing pass is INFO; a pass that only skipped
/// dead/automation backlog is DEBUG so it stays quiet at default log levels
/// but is there when diagnosing; a fully idle pass logs nothing.
fn log_pass(outcome: &SweepOutcome) {
    if !outcome.errors.is_empty() {
        tracing::warn!(
            filed = outcome.filed,
            file_errors = outcome.errors.len(),
            first_error = outcome.errors.first().map(String::as_str).unwrap_or(""),
            skipped_terminal = outcome.skipped_terminal,
            skipped_not_work_item = outcome.skipped_not_work_item,
            skipped_missing = outcome.skipped_missing,
            "dispatch stall attention sweep: some stalls failed to file"
        );
    } else if outcome.filed > 0 {
        tracing::info!(
            filed = outcome.filed,
            skipped_terminal = outcome.skipped_terminal,
            skipped_not_work_item = outcome.skipped_not_work_item,
            skipped_missing = outcome.skipped_missing,
            "dispatch stall attention sweep: filed/refreshed attention items"
        );
    } else if outcome.skipped_total() > 0 {
        tracing::debug!(
            skipped_terminal = outcome.skipped_terminal,
            skipped_not_work_item = outcome.skipped_not_work_item,
            skipped_missing = outcome.skipped_missing,
            "dispatch stall attention sweep: no live work-item stalls to escalate"
        );
    }
}

/// Per-pass tally of what the sweep did, so [`log_pass`] can emit exactly
/// one aggregate line instead of one per stall. Every stall the sweep skips
/// is counted by reason here; see [`StallEscalation`] for what each skip
/// class means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    /// Attention items filed or refreshed this pass.
    pub filed: usize,
    /// Stalls bound to a non-work-item id (an automation-triage execution's
    /// `auto_…` id) — nothing to attach a kanban attention item to. See
    /// [`StallEscalation::NotWorkItem`].
    pub skipped_not_work_item: usize,
    /// Stalls whose execution row is already terminal — a dead timeline, not
    /// a live stall. See [`StallEscalation::Terminal`].
    pub skipped_terminal: usize,
    /// Stalls whose execution row no longer exists (retention-swept, or a
    /// mirror that outlived its row). See [`StallEscalation::Missing`].
    pub skipped_missing: usize,
    /// Errors hit while classifying or filing a stall this pass — expected to
    /// be empty once the liveness/id filters run; a non-empty list is a
    /// genuine problem. Kept as messages (not just a count) so the single
    /// aggregate log line can name the first failure without per-item spam;
    /// `errors.len()` is the count.
    pub errors: Vec<String>,
}

impl SweepOutcome {
    /// Total stalls skipped this pass across every skip reason.
    fn skipped_total(&self) -> usize {
        self.skipped_not_work_item + self.skipped_terminal + self.skipped_missing
    }

    fn record_error(&mut self, err: impl std::fmt::Display) {
        self.errors.push(err.to_string());
    }
}

/// Run a single pass: find every persistently-stalled execution and, for
/// each one that is still live and bound to a real work item, idempotently
/// upsert its attention item. Executions that are already terminal, gone,
/// or bound to a non-work-item id (an automation) are skipped — the file-
/// scan detector can't tell those apart, so the authoritative call is made
/// here against the DB (see [`WorkDb::classify_dispatch_stall`]). Returns a
/// [`SweepOutcome`] tallying what was filed and what was skipped, by reason.
pub fn run_one_pass(root: &Path, work_db: &WorkDb, threshold: Duration) -> anyhow::Result<SweepOutcome> {
    let now_ms = boss_engine_utils::epoch_time::now_epoch_ms();
    let stalls = dispatch_reader::persistently_stalled(root, now_ms, threshold.as_millis())?;
    let mut outcome = SweepOutcome::default();
    for stall in stalls {
        // Classify by execution id against the DB — its `work_item_id` and
        // `status` are the truth, unlike the possibly-stale mirror line.
        let work_item_id = match work_db.classify_dispatch_stall(&stall.execution_id) {
            Ok(StallEscalation::Escalate { work_item_id }) => work_item_id,
            Ok(StallEscalation::NotWorkItem) => {
                outcome.skipped_not_work_item += 1;
                continue;
            }
            Ok(StallEscalation::Terminal) => {
                outcome.skipped_terminal += 1;
                continue;
            }
            Ok(StallEscalation::Missing) => {
                outcome.skipped_missing += 1;
                continue;
            }
            Err(err) => {
                outcome.record_error(err);
                continue;
            }
        };
        if let Err(err) = work_db.file_dispatch_stage_stalled_attention(
            &work_item_id,
            &stall.execution_id,
            &stall.stalled_stage,
            (stall.elapsed_in_stage_ms / 1000) as u64,
            threshold.as_secs(),
        ) {
            outcome.record_error(err);
            continue;
        }
        outcome.filed += 1;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::dispatch_events::{DispatchEvent, DispatchEventSink, JsonlFileSink, Outcome, Stage};
    use crate::test_support::*;
    use crate::work::DISPATCH_STAGE_STALLED_ATTENTION_KIND;
    use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};

    /// Emit a single stalled (non-terminal, timestamp-0) dispatch event for
    /// `execution_id` into a fresh state root, so `persistently_stalled`
    /// reports it once `now_ms` is past the threshold. Returns the root dir
    /// (kept alive by the caller).
    async fn emit_stalled_event(execution_id: &str) -> TempDir {
        let root = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(root.path());
        let mut event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, execution_id);
        event.ts_epoch_ms = 0;
        sink.emit(event).await;
        root
    }

    #[tokio::test]
    async fn run_one_pass_files_attention_item_for_persistent_stall() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_test_chore(&db, &product_id, "stuck chore").id;
        // A live (non-terminal) execution row is required now: the sweep
        // consults the DB for liveness, not just the on-disk mirror.
        let execution = create_ready_chore_execution(&db, work_item_id.clone());
        let root = emit_stalled_event(&execution.id).await;

        // now_ms far past the threshold.
        let outcome = run_one_pass(root.path(), &db, Duration::from_millis(300_000)).unwrap();
        assert_eq!(outcome.filed, 1);
        assert_eq!(outcome.skipped_total(), 0);

        let items = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, DISPATCH_STAGE_STALLED_ATTENTION_KIND);
        assert_eq!(items[0].status, "open");
        assert!(items[0].title.contains("cube_change_created"), "{:?}", items[0].title);
    }

    #[tokio::test]
    async fn run_one_pass_refreshes_rather_than_duplicates_on_repeat_ticks() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_test_chore(&db, &product_id, "stuck chore").id;
        let execution = create_ready_chore_execution(&db, work_item_id.clone());
        let root = emit_stalled_event(&execution.id).await;

        run_one_pass(root.path(), &db, Duration::from_millis(300_000)).unwrap();
        run_one_pass(root.path(), &db, Duration::from_millis(300_000)).unwrap();

        let items = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(items.len(), 1, "repeat ticks must refresh, not duplicate");
    }

    #[tokio::test]
    async fn run_one_pass_skips_executions_under_threshold() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_test_chore(&db, &product_id, "fresh chore").id;
        let execution = create_ready_chore_execution(&db, work_item_id.clone());

        let root = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(root.path());
        // Fresh event — `persistently_stalled` computes elapsed against
        // `now_epoch_ms()` internally, so a just-emitted event is always
        // under any positive threshold.
        let event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, &execution.id);
        sink.emit(event).await;

        let outcome = run_one_pass(root.path(), &db, Duration::from_secs(300)).unwrap();
        assert_eq!(outcome.filed, 0);
        assert_eq!(outcome.skipped_total(), 0);
        assert!(db.list_attention_items_for_work_item(&work_item_id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_one_pass_skips_terminal_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_test_chore(&db, &product_id, "dead chore").id;
        // An execution that reached a terminal state: its dispatch mirror may
        // still be on disk, but escalating it would re-file forever.
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Failed)
                    .build(),
            )
            .unwrap();
        let root = emit_stalled_event(&execution.id).await;

        let outcome = run_one_pass(root.path(), &db, Duration::from_millis(300_000)).unwrap();
        assert_eq!(outcome.filed, 0);
        assert_eq!(outcome.skipped_terminal, 1);
        assert!(outcome.errors.is_empty());
        assert!(db.list_attention_items_for_work_item(&work_item_id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_one_pass_skips_automation_execution_without_error() {
        // The regression this module was filed for: an `automation_triage`
        // execution carries an `auto_…` id as its `work_item_id`, which the
        // attention layer can't target. It must be counted and skipped — NOT
        // produce a per-item error/warn on every pass.
        let (_dir, db) = open_db();
        let execution = db
            .create_automation_triage_execution("auto_test", "git@github.com:spinyfin/mono.git")
            .unwrap();
        assert_eq!(execution.work_item_id, "auto_test");
        let root = emit_stalled_event(&execution.id).await;

        let outcome = run_one_pass(root.path(), &db, Duration::from_millis(300_000)).unwrap();
        assert_eq!(outcome.filed, 0);
        assert_eq!(outcome.skipped_not_work_item, 1);
        assert!(outcome.errors.is_empty(), "automation stalls must not error the sweep");
    }

    #[tokio::test]
    async fn run_one_pass_skips_stall_with_no_execution_row() {
        // A dispatch mirror that outlived its DB row (retention-swept) must
        // not keep being escalated.
        let (_dir, db) = open_db();
        let root = emit_stalled_event("exec_gone_000").await;

        let outcome = run_one_pass(root.path(), &db, Duration::from_millis(300_000)).unwrap();
        assert_eq!(outcome.filed, 0);
        assert_eq!(outcome.skipped_missing, 1);
        assert!(outcome.errors.is_empty());
    }
}
