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
use crate::work::WorkDb;

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
                Ok(filed) if filed > 0 => {
                    tracing::info!(filed, "dispatch stall attention sweep: filed/refreshed attention items");
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(?err, "dispatch stall attention sweep: pass failed"),
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single pass: find every persistently-stalled execution and
/// idempotently upsert its attention item. Returns the number of items
/// filed/refreshed.
pub fn run_one_pass(root: &Path, work_db: &WorkDb, threshold: Duration) -> anyhow::Result<usize> {
    let now_ms = boss_engine_utils::epoch_time::now_epoch_ms();
    let stalls = dispatch_reader::persistently_stalled(root, now_ms, threshold.as_millis())?;
    let mut filed = 0;
    for stall in stalls {
        let Some(work_item_id) = &stall.work_item_id else {
            // Nothing to attach an attention item to — a dispatch
            // timeline with no work_item_id is a pathological case the
            // DB-side attention surface can't target. Log and move on;
            // the execution is still visible via `bossctl dispatch
            // ghost-active --include-stalled`.
            tracing::warn!(
                execution_id = %stall.execution_id,
                stage = %stall.stalled_stage,
                "dispatch stall attention sweep: stalled execution has no work_item_id; cannot file attention item"
            );
            continue;
        };
        if let Err(err) = work_db.file_dispatch_stage_stalled_attention(
            work_item_id,
            &stall.execution_id,
            &stall.stalled_stage,
            (stall.elapsed_in_stage_ms / 1000) as u64,
            threshold.as_secs(),
        ) {
            tracing::warn!(
                work_item_id = %work_item_id,
                execution_id = %stall.execution_id,
                ?err,
                "dispatch stall attention sweep: failed to file attention item"
            );
            continue;
        }
        filed += 1;
    }
    Ok(filed)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::dispatch_events::{DispatchEvent, DispatchEventSink, JsonlFileSink, Outcome, Stage};
    use crate::test_support::*;
    use crate::work::DISPATCH_STAGE_STALLED_ATTENTION_KIND;

    #[tokio::test]
    async fn run_one_pass_files_attention_item_for_persistent_stall() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_test_chore(&db, &product_id, "stuck chore").id;

        let root = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(root.path());
        let mut event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-stuck");
        event.ts_epoch_ms = 0;
        event.work_item_id = Some(work_item_id.clone());
        sink.emit(event).await;

        // now_ms far past the threshold.
        let filed = run_one_pass(root.path(), &db, Duration::from_millis(300_000)).unwrap();
        assert_eq!(filed, 1);

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

        let root = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(root.path());
        let mut event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-stuck");
        event.ts_epoch_ms = 0;
        event.work_item_id = Some(work_item_id.clone());
        sink.emit(event).await;

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

        let root = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(root.path());
        // Fresh event — `persistently_stalled` computes elapsed against
        // `now_epoch_ms()` internally, so a just-emitted event is always
        // under any positive threshold.
        let mut event = DispatchEvent::new(Stage::CubeChangeCreated, Outcome::Ok, "exec-fresh");
        event.work_item_id = Some(work_item_id.clone());
        sink.emit(event).await;

        let filed = run_one_pass(root.path(), &db, Duration::from_secs(300)).unwrap();
        assert_eq!(filed, 0);
        assert!(db.list_attention_items_for_work_item(&work_item_id).unwrap().is_empty());
    }
}
