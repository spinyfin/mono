//! Measured work, memory proxy and actual JSON delta bytes on the existing
//! production-cardinality fixture. Counts are test points, not hub capacities.
use super::*;
use crate::population_timing::PopulationTrace;
use crate::work::{board_caches::BoardCaches, board_projection::Refresh};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn resources() -> (f64, i64) {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the supplied rusage on success.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) }, 0);
    let usage = unsafe { usage.assume_init() };
    let cpu = (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64 * 1000.0
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1000.0;
    let rss_bytes = if cfg!(target_os = "macos") {
        usage.ru_maxrss
    } else {
        usage.ru_maxrss * 1024
    };
    (cpu, rss_bytes)
}

#[test]
fn board_projection_scale_measurement() {
    let (_dir, path) = disk_db_path("projection-scale");
    let db = WorkDb::open(path).unwrap();
    super::work_tree_scale_tests::seed(&db.connect().unwrap());
    let product = "scale-product-0";
    let samples: usize = std::env::var("BOSS_BENCH_SAMPLES")
        .map(|s| s.parse().unwrap())
        .unwrap_or(1);
    assert!(samples > 0);
    let batches: &[usize] = if std::env::var_os("BOSS_BENCH_SAMPLES").is_some() {
        &[8, 32, 64]
    } else {
        &[8]
    };
    for &workers in batches {
        println!("projection setup workers={workers}");
        let cache = BoardCaches::default();
        let mut model = None;
        let cold_start = Instant::now();
        let mut trace = PopulationTrace::new(product, "cold-scale", None, cold_start);
        let (initial, _) = db
            .refresh_board_projection(product, &mut model, &mut trace, &cache, 100)
            .unwrap();
        println!(
            "cold_scale build_ms={:.3} tasks={} chores={} descriptions={} runtimes={:?} tasks_stage={:?} reviews={:?} assemble={:?}",
            cold_start.elapsed().as_secs_f64() * 1000.0,
            initial.tasks.len(),
            initial.chores.len(),
            initial
                .tasks
                .iter()
                .chain(&initial.chores)
                .map(|t| t.description.len())
                .sum::<usize>(),
            trace.segment_counts("db.task_runtimes"),
            trace.segment_counts("db.tasks"),
            trace.segment_counts("db.ai_review_state"),
            trace.segment_counts("assemble")
        );
        let mut wire = boss_protocol::work_tree_delta::WorkTreeCache::default();
        let snapshot = wire.update(&initial).unwrap();
        let snapshot_bytes = serde_json::to_vec(&snapshot).unwrap().len();
        let pages = snapshot.into_pages(512 * 1024).unwrap();
        let max_page_bytes = pages
            .iter()
            .map(|page| serde_json::to_vec(page).unwrap().len())
            .max()
            .unwrap();
        println!(
            "cold_scale total_ms={:.3} full_bytes={snapshot_bytes} pages={} max_page_bytes={max_page_bytes}",
            cold_start.elapsed().as_secs_f64() * 1000.0,
            pages.len()
        );
        assert!(max_page_bytes < 513 * 1024);
        drop(initial);
        for sample in 0..samples {
            // One changed card per worker, including revision family effects.
            // Alternate structural status changes and execution/runtime writes.
            let write_cpu = resources().0;
            let write_start = Instant::now();
            let mut conn = db.connect().unwrap();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            for i in 0..workers {
                let id = format!("scale-task-{}", 7756 + i);
                tx.execute(
                    "UPDATE tasks SET status = ?2, name = ?3 WHERE id = ?1",
                    params![
                        id,
                        if sample % 2 == 0 { "in_review" } else { "active" },
                        format!("worker {i} sample {sample}")
                    ],
                )
                .unwrap();
                tx.execute(
                    "UPDATE work_executions SET status = ?2 WHERE id = (SELECT id FROM work_executions WHERE work_item_id = ?1 ORDER BY created_at DESC, id DESC LIMIT 1)",
                    params![id, if sample % 2 == 0 { "running" } else { "waiting_human" }],
                )
                .unwrap();
            }
            tx.commit().unwrap();
            drop(conn);
            let write_ms = write_start.elapsed().as_secs_f64() * 1000.0;
            let write_cpu_ms = resources().0 - write_cpu;
            let cpu = resources().0;
            let start = Instant::now();
            let (incremental, refresh) = db
                .refresh_board_projection(product, &mut model, &mut PopulationTrace::disabled(), &cache, 100)
                .unwrap();
            let incremental_ms = start.elapsed().as_secs_f64() * 1000.0;
            let incremental_cpu_ms = resources().0 - cpu;
            let Refresh::Incremental(cards) = refresh else {
                panic!("unexpected recovery: {refresh:?}");
            };
            assert!(cards <= 2 * workers);
            let cpu = resources().0;
            let start = Instant::now();
            let baseline = db
                .get_work_tree_cached_at(product, &mut PopulationTrace::disabled(), &cache, 100)
                .unwrap();
            let baseline_ms = start.elapsed().as_secs_f64() * 1000.0;
            let baseline_cpu_ms = resources().0 - cpu;
            assert_eq!(
                serde_json::to_value(&incremental).unwrap(),
                serde_json::to_value(&baseline).unwrap()
            );
            // Also retain the independent uncached oracle in every sequence.
            println!("projection validating uncached workers={workers} sample={sample}");
            assert_eq!(
                serde_json::to_value(&baseline).unwrap(),
                serde_json::to_value(db.get_work_tree_at(product, 100).unwrap()).unwrap()
            );
            let start = Instant::now();
            let delta_bytes = serde_json::to_vec(&wire.update(&incremental).unwrap()).unwrap().len();
            let diff_wire_ms = start.elapsed().as_secs_f64() * 1000.0;
            let peak_rss_bytes = resources().1;
            let retained_bytes = model.as_ref().unwrap().retained_bytes;
            println!(
                "projection workers={workers} sample={sample} cards={cards} write_wall_ms={write_ms:.3} write_cpu_ms={write_cpu_ms:.3} incremental_wall_ms={incremental_ms:.3} incremental_cpu_ms={incremental_cpu_ms:.3} cached_full_wall_ms={baseline_ms:.3} cached_full_cpu_ms={baseline_cpu_ms:.3} diff_wire_ms={diff_wire_ms:.3} delta_bytes={delta_bytes} snapshot_bytes={snapshot_bytes} retained_bound_bytes={retained_bytes} process_peak_rss_bytes={peak_rss_bytes}"
            );
            assert!(delta_bytes < snapshot_bytes / 10);
        }
    }
}

#[test]
fn attention_reconcile_scale_measurement() {
    let logs = crate::test_support::log_capture::install();
    let (_dir, path) = disk_db_path("reconcile-scale");
    let db = WorkDb::open(path).unwrap();
    let conn = db.connect().unwrap();
    super::work_tree_scale_tests::seed(&conn);
    conn.execute_batch("UPDATE work_runs SET started_at = '1';
        INSERT INTO work_attention_items(id, work_item_id, kind, status, title, body_markdown, created_at)
        SELECT 'scale-attention-' || id, id, 'churn_guard_parked', 'open', 'scale', 'scale', '2' FROM tasks WHERE product_id = 'scale-product-0';").unwrap();
    let legacy = "UPDATE work_attention_items SET status = 'resolved', resolved_at = '3'
        WHERE status = 'open' AND kind = 'churn_guard_parked' AND EXISTS (
        SELECT 1 FROM work_runs r JOIN work_executions e ON e.id = r.execution_id
        WHERE e.work_item_id = COALESCE(work_attention_items.work_item_id,
          (SELECT we.work_item_id FROM work_executions we WHERE we.id = work_attention_items.execution_id))
        AND r.started_at IS NOT NULL AND CAST(r.started_at AS INTEGER) >= CAST(COALESCE(work_attention_items.last_raised_at, work_attention_items.created_at) AS INTEGER)
        AND r.execution_id IS NOT work_attention_items.execution_id)";
    let start = Instant::now();
    assert_eq!(conn.execute(legacy, []).unwrap(), 0);
    println!(
        "reconcile_scale legacy_hold_ms={:.3}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    drop(conn);
    let start = Instant::now();
    let result = db.reconcile_stale_attention_signals();
    println!(
        "reconcile_scale batched_total_ms={:.3}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    for line in String::from_utf8_lossy(&logs.lock())
        .lines()
        .filter(|line| line.contains("attention reconcile batch timing"))
    {
        println!("{line}");
    }
    assert!(result.unwrap().is_empty());
}

/// Socket-inclusive cold first-snapshot measurement on the production-cardinality
/// fixture: request line in, final snapshot frame out, through request
/// admission, the shared read connection, the session queue and the writer.
/// Prints per-mode latency, frame sizes and the engine's own population stages.
struct Measured {
    elapsed_ms: f64,
    frames: usize,
    total_bytes: usize,
    max_frame_bytes: usize,
}

async fn first_board(engine: &crate::app::MeasurementEngine, request_id: &str, payload: serde_json::Value) -> Measured {
    let (client, handler) = engine.serve();
    let (read, mut write) = client.into_split();
    let mut lines = tokio::io::BufReader::new(read).lines();
    let request = serde_json::json!({ "request_id": request_id, "payload": payload });
    let start = std::time::Instant::now();
    write.write_all(format!("{request}\n").as_bytes()).await.unwrap();
    let mut measured = Measured {
        elapsed_ms: 0.0,
        frames: 0,
        total_bytes: 0,
        max_frame_bytes: 0,
    };
    loop {
        let line = lines.next_line().await.unwrap().unwrap();
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        if frame["request_id"] != request_id {
            continue;
        }
        measured.frames += 1;
        measured.total_bytes += line.len();
        measured.max_frame_bytes = measured.max_frame_bytes.max(line.len());
        let page = &frame["payload"]["update"]["changes"]["snapshot_page"];
        let paged_mid = page["index"]
            .as_u64()
            .zip(page["count"].as_u64())
            .is_some_and(|(i, n)| i + 1 < n);
        if !paged_mid {
            measured.elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            break;
        }
    }
    drop(lines);
    drop(write);
    handler.await.unwrap().unwrap();
    measured
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_snapshot_end_to_end_measurement() {
    let diagnostics = tempfile::tempdir().unwrap();
    crate::population_timing::install_global(Some(diagnostics.path().to_path_buf()));
    let modes = [
        (
            "single_frame",
            serde_json::json!({"delta": false, "snapshot_pages": false}),
        ),
        ("paged", serde_json::json!({"delta": true, "snapshot_pages": true})),
    ];
    for (mode, flags) in modes {
        // A fresh engine state per mode, so sample 0 is a genuinely cold first
        // snapshot (no shared board cache, no session cache); later samples are warm.
        let engine = crate::app::MeasurementEngine::new();
        super::work_tree_scale_tests::seed(&engine.db().connect().unwrap());
        for sample in 0..3 {
            let mut payload = serde_json::json!({"type": "get_work_tree", "product_id": "scale-product-0"});
            payload
                .as_object_mut()
                .unwrap()
                .extend(flags.as_object().unwrap().clone());
            let request_id = format!("{mode}-{sample}");
            let m = first_board(&engine, &request_id, payload).await;
            println!(
                "e2e_cold mode={mode} sample={sample} {} elapsed_ms={:.1} frames={} total_bytes={} max_frame_bytes={}",
                if sample == 0 { "cold" } else { "warm" },
                m.elapsed_ms,
                m.frames,
                m.total_bytes,
                m.max_frame_bytes
            );
            assert!(m.frames >= 1 && m.total_bytes > 0);
            if mode == "paged" {
                assert!(m.max_frame_bytes < 513 * 1024 + 4096, "frame bound");
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // One line per (request, stage): per-page stages are summed over the pages.
    let mut stages: Vec<(String, String, f64, i64, usize)> = Vec::new();
    for entry in std::fs::read_dir(diagnostics.path()).unwrap() {
        for line in std::fs::read_to_string(entry.unwrap().path()).unwrap().lines() {
            let rec: serde_json::Value = serde_json::from_str(line).unwrap();
            let (Some(request), Some(segment)) = (rec["request_id"].as_str(), rec["segment"].as_str()) else {
                continue;
            };
            if !request.ends_with("-0") {
                continue;
            }
            let ms = rec["duration_ms"].as_f64().unwrap_or(0.0);
            let bytes = rec["payload_bytes"].as_i64().unwrap_or(0);
            match stages.iter_mut().find(|s| s.0 == request && s.1 == segment) {
                Some(stage) => {
                    stage.2 += ms;
                    stage.3 += bytes;
                    stage.4 += 1;
                }
                None => stages.push((request.to_owned(), segment.to_owned(), ms, bytes, 1)),
            }
        }
    }
    for (request, segment, ms, bytes, count) in stages {
        println!(
            "e2e_stage request={request} segment={segment} duration_ms={ms:.1} payload_bytes={bytes} count={count}"
        );
    }
}
