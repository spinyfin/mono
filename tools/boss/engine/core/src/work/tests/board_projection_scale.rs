//! Measured work, memory proxy and actual JSON delta bytes on the existing
//! production-cardinality fixture. Counts are test points, not hub capacities.
use super::*;
use crate::population_timing::PopulationTrace;
use crate::work::{board_caches::BoardCaches, board_projection::Refresh};
use std::time::Instant;

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
        let (initial, _) = db
            .refresh_board_projection(product, &mut model, &mut PopulationTrace::disabled(), &cache, 100)
            .unwrap();
        let mut wire = boss_protocol::work_tree_delta::WorkTreeCache::default();
        let snapshot_bytes = serde_json::to_vec(&wire.update(&initial).unwrap()).unwrap().len();
        drop(initial);
        for sample in 0..samples {
            // One changed card per worker, including revision family effects.
            // Alternate structural status changes and execution/runtime writes.
            let write_cpu = resources().0;
            let write_start = Instant::now();
            let mut conn = db.connect().unwrap();
            let tx = conn.transaction().unwrap();
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
