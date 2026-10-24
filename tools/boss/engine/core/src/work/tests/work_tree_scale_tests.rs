//! Deterministic, production-cardinality fixture; never reads operator state.
use super::*;
use crate::population_timing::{PopulationTrace, segment};
use std::time::Instant;

fn seed(conn: &Connection) {
    conn.execute_batch("BEGIN").unwrap();
    for i in 0..5 {
        conn.execute(
            "INSERT INTO products(id,name,slug,status,created_at,updated_at) VALUES (?1,?1,?1,'active','1','1')",
            [format!("scale-product-{i}")],
        )
        .unwrap();
    }
    for i in 0..131 {
        conn.execute("INSERT INTO projects(id,product_id,name,slug,status,priority,created_at,updated_at) VALUES (?1,'scale-product-0',?1,?1,'active','medium','1','1')", [format!("scale-project-{i}")]).unwrap();
    }
    // Put the large board in one product; reserve four small empty products.
    // Fixed 3 KiB briefs approximate the observed multi-megabyte tree without
    // claiming to reproduce unknown production text or revision topology.
    let description = "Synthetic work item description. ".repeat(96);
    for i in 0..8030 {
        let status = match i {
            0..6640 => "done",
            6640..7085 => "archived",
            7085..7435 => "in_review",
            7435..7756 => "todo",
            7756..7953 => "blocked",
            _ => "active",
        };
        let parent = (i % 4 == 3).then(|| format!("scale-task-{}", i - 1));
        let kind = if parent.is_some() { "revision" } else { "project_task" };
        conn.execute("INSERT INTO tasks(id,product_id,project_id,kind,name,description,status,created_at,updated_at,pr_url,pr_head_sha,parent_task_id,completed_at) VALUES (?1,'scale-product-0',?2,?3,?1,?4,?5,'1','1',?6,'synthetic-head',?7,?8)", params![format!("scale-task-{i}"),format!("scale-project-{}",(i / 4) % 131),kind,description,status,format!("https://github.com/example/fixture/pull/{}",i + 1),parent,(status == "done").then_some("1")]).unwrap();
    }
    for i in 0..21127 {
        conn.execute("INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at,finished_at) VALUES (?1,?2,'pr_review','completed','https://github.com/example/fixture','1','2')", params![format!("scale-exec-{i}"),format!("scale-task-{}",if i < 1818 { i * 4 } else { i % 8030 })]).unwrap();
    }
    for i in 0..19703 {
        conn.execute("INSERT INTO work_runs(id,execution_id,agent_id,status,created_at,finished_at) VALUES (?1,?2,'synthetic','completed','1','2')", params![format!("scale-run-{i}"),format!("scale-exec-{i}")]).unwrap();
    }
    for i in 0..4131 {
        conn.execute("INSERT INTO worker_proposals(id,execution_id,work_item_id,kind,payload_json,idempotency_key,state,created_at) VALUES (?1,?2,?3,'done','{}',?1,'applied','1')", params![format!("scale-proposal-{i}"),format!("scale-exec-{i}"),format!("scale-task-{}",if i < 1818 { i * 4 } else { i % 8030 })]).unwrap();
    }
    for i in 0..786 {
        conn.execute("INSERT INTO pr_review_batches(id,cycle_root_id,base_sha,classification_json,created_at,phase,pr_number,pr_url,status,target_sha,updated_at) VALUES (?1,?2,'base','{}','1','pre_merge',?3,'https://github.com/example/fixture/pull/1','completed','synthetic-head','2')", params![format!("scale-batch-{i}"),format!("scale-task-{}",i * 4),i + 1]).unwrap();
    }
    for i in 0..1818 {
        conn.execute("INSERT INTO pr_review_verdicts(id,execution_id,work_item_id,head_sha,gate_outcome,created_at) VALUES (?1,?2,?3,'synthetic-head','completed_clean','2')", params![format!("scale-verdict-{i}"),format!("scale-exec-{i}"),format!("scale-task-{}",i * 4)]).unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
}

#[test]
fn work_tree_production_scale_measurement() {
    let (_dir, path) = disk_db_path("scale");
    let db = WorkDb::open(path).unwrap();
    let conn = db.connect().unwrap();
    seed(&conn);
    for (table, expected) in [
        ("tasks", 8030),
        ("projects", 131),
        ("products", 5),
        ("work_executions", 21127),
        ("work_runs", 19703),
        ("worker_proposals", 4131),
        ("pr_review_batches", 786),
        ("pr_review_verdicts", 1818),
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, expected, "{table}");
    }
    assert!(!conn.prepare("PRAGMA foreign_key_check").unwrap().exists([]).unwrap());
    drop(conn);
    measure(&db, "scale-product-0", Some(7585));
}

fn measure(db: &WorkDb, product: &str, expected: Option<usize>) {
    let samples: usize = std::env::var("BOSS_BENCH_SAMPLES")
        .map(|value| value.parse().expect("BOSS_BENCH_SAMPLES must be an integer"))
        .unwrap_or(1);
    assert!(samples > 0, "at least one sample is required");
    let mut totals = Vec::new();
    let mut reviews = Vec::new();
    for sample in 0..samples {
        let start = Instant::now();
        let mut trace = PopulationTrace::new(product, "scale", Some(i64::try_from(sample).unwrap()), start);
        let tree = db.get_work_tree_instrumented(product, &mut trace).unwrap();
        let bytes = serde_json::to_vec(&tree).unwrap().len();
        totals.push(start.elapsed().as_secs_f64() * 1000.0);
        reviews.push(trace.segment_counts(segment::DB_AI_REVIEW_STATE).unwrap().0);
        println!(
            "scale sample={sample} fetch_db_and_serialize_ms={:.3} ai_review_state_ms={:.3}",
            totals[sample], reviews[sample]
        );
        if let Some(expected) = expected {
            assert_eq!(tree.tasks.len() + tree.chores.len(), expected);
            assert!(
                tree.tasks
                    .iter()
                    .any(|task| task.ai_review_state.as_deref() == Some(AI_REVIEW_STATE_REVIEWED_CLEAN_PENDING)),
                "the review query must succeed, not silently fall back to empty badges"
            );
        }
        if sample == 0 {
            let descriptions: usize = tree.tasks.iter().chain(&tree.chores).map(|t| t.description.len()).sum();
            println!(
                "scale payload_bytes={bytes} description_bytes={descriptions} returned={}",
                tree.tasks.len() + tree.chores.len()
            );
            println!(
                "scale runtime_queries={:?}",
                trace.segment_counts(segment::DB_TASK_RUNTIMES)
            );
            let hidden_revisions = tree
                .tasks
                .iter()
                .chain(&tree.chores)
                .filter(|t| t.kind == TaskKind::Revision && matches!(t.status, TaskStatus::Done | TaskStatus::InReview))
                .count();
            let done = tree
                .tasks
                .iter()
                .chain(&tree.chores)
                .filter(|t| t.status == TaskStatus::Done)
                .count();
            println!("scale done={done} hidden_terminal_revisions={hidden_revisions}");
            for count in [1, 10, 100] {
                let records: Vec<_> = tree.tasks.iter().chain(&tree.chores).take(count).collect();
                println!(
                    "scale candidate_raw_records={count} bytes={}",
                    serde_json::to_vec(&records).unwrap().len()
                );
            }
        }
    }
    totals.sort_by(f64::total_cmp);
    reviews.sort_by(f64::total_cmp);
    let p50 = (samples * 50).div_ceil(100) - 1;
    let p95 = (samples * 95).div_ceil(100) - 1;
    println!(
        "scale samples={samples} fetch_db_and_serialize_ms p50={:.3} p95={:.3}; ai_review_state_ms p50={:.3} p95={:.3}",
        totals[p50], totals[p95], reviews[p50], reviews[p95]
    );
    if expected.is_some() {
        measure_runtime_patches(db, product);
        measure_deltas(db, product);
        measure_cached_runtimes(db, product, samples);
    }
}

/// Per-item runtime cache: the same assembly, with runtimes served from the
/// epoch-validated cache. Every cached tree must equal the uncached one, so the
/// timings below compare identical outputs.
fn measure_cached_runtimes(db: &WorkDb, product: &str, samples: usize) {
    use crate::work::board_runtime_cache::BoardRuntimeCache;
    let assemble = |cache: Option<&BoardRuntimeCache>, label: &str| {
        let start = Instant::now();
        let mut trace = PopulationTrace::new(product, "scale-cache", None, start);
        let tree = match cache {
            Some(cache) => db.get_work_tree_cached(product, &mut trace, cache),
            None => db.get_work_tree_instrumented(product, &mut trace),
        }
        .unwrap();
        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        let (runtime_ms, _, queries) = trace.segment_counts(segment::DB_TASK_RUNTIMES).unwrap();
        println!(
            "scale runtime_cache pass={label} assemble_ms={total_ms:.3} task_runtimes_ms={runtime_ms:.3} \
             runtime_queries={}",
            queries.unwrap()
        );
        (tree, total_ms, runtime_ms)
    };
    let runtimes = |tree: &boss_protocol::WorkTree| serde_json::to_value(&tree.task_runtimes).unwrap();

    let (reference, _, _) = assemble(None, "uncached");
    let cache = BoardRuntimeCache::default();
    let (cold, _, _) = assemble(Some(&cache), "cold");
    assert_eq!(runtimes(&cold), runtimes(&reference), "cold cached runtimes diverged");
    let stats = cache.stats();
    println!(
        "scale runtime_cache entries={} approx_bytes={} hits={} misses={}",
        stats.entries, stats.approx_bytes, stats.hits, stats.misses
    );

    let mut warm = Vec::new();
    let mut warm_runtime = Vec::new();
    let mut uncached = Vec::new();
    let mut uncached_runtime = Vec::new();
    for sample in 0..samples {
        let (tree, total_ms, runtime_ms) = assemble(Some(&cache), &format!("warm-{sample}"));
        assert_eq!(runtimes(&tree), runtimes(&reference), "warm cached runtimes diverged");
        warm.push(total_ms);
        warm_runtime.push(runtime_ms);
        let (_, total_ms, runtime_ms) = assemble(None, &format!("uncached-{sample}"));
        uncached.push(total_ms);
        uncached_runtime.push(runtime_ms);
    }
    let p50 = |values: &mut Vec<f64>| {
        values.sort_by(f64::total_cmp);
        values[(values.len() * 50).div_ceil(100) - 1]
    };
    println!(
        "scale runtime_cache samples={samples} assemble_ms p50 uncached={:.3} warm={:.3}; \
         task_runtimes_ms p50 uncached={:.3} warm={:.3}",
        p50(&mut uncached),
        p50(&mut warm),
        p50(&mut uncached_runtime),
        p50(&mut warm_runtime)
    );

    // Writing N executions recomputes exactly the N items they belong to.
    // Execution `i < 1818` belongs to task `4 * i`, so these items are distinct.
    for count in [1usize, 10, 100] {
        let conn = db.connect().unwrap();
        for i in 0..count {
            conn.execute(
                "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
                params![format!("scale-exec-{i}"), format!("{}", 3 + count)],
            )
            .unwrap();
        }
        drop(conn);
        let misses_before = cache.stats().misses;
        let (tree, _, _) = assemble(Some(&cache), &format!("after-{count}-writes"));
        let (reference, _, _) = assemble(None, &format!("uncached-after-{count}-writes"));
        assert_eq!(
            runtimes(&tree),
            runtimes(&reference),
            "{count} writes: cached runtimes diverged"
        );
        assert_eq!(cache.stats().misses - misses_before, count as u64, "{count} writes");
    }
}

/// The cost of answering a runtime-class invalidation (hub-and-spoke design
/// §2.4): a patch of the named cards, against the full assembly above that
/// every invalidation costs without the split. Also pins, at production
/// cardinality, that a patched card is the card the tree produces.
fn measure_runtime_patches(db: &WorkDb, product: &str) {
    use std::collections::HashMap;
    let samples: usize = std::env::var("BOSS_BENCH_SAMPLES")
        .map(|value| value.parse().expect("BOSS_BENCH_SAMPLES must be an integer"))
        .unwrap_or(1)
        .max(5);
    let tree = db.get_work_tree(product).unwrap();
    let tree_cards: HashMap<&str, serde_json::Value> = tree
        .tasks
        .iter()
        .chain(&tree.chores)
        .map(|task| (task.id.as_str(), serde_json::to_value(task).unwrap()))
        .collect();
    // The cards runtime changes land on: in review, running or queued.
    let live: Vec<String> = tree
        .tasks
        .iter()
        .chain(&tree.chores)
        .filter(|task| {
            matches!(
                task.status,
                TaskStatus::InReview | TaskStatus::Active | TaskStatus::Todo
            )
        })
        .map(|task| task.id.clone())
        .collect();
    for count in [1usize, 10, 100] {
        let ids: Vec<String> = live.iter().take(count).cloned().collect();
        assert_eq!(ids.len(), count, "the fixture must have {count} live cards");
        let mut millis = Vec::new();
        let mut bytes = 0;
        let mut returned = 0;
        for _ in 0..samples {
            let start = Instant::now();
            let patch = db.get_work_runtime_patch(product, &ids).unwrap();
            bytes = serde_json::to_vec(&patch.items).unwrap().len();
            millis.push(start.elapsed().as_secs_f64() * 1000.0);
            returned = patch.items.len();
            assert!(
                patch.absent.is_empty(),
                "live cards are never absent: {:?}",
                patch.absent
            );
            for entry in &patch.items {
                let (WorkItem::Task(task) | WorkItem::Chore(task)) = &entry.item else {
                    panic!("a patch carries only tasks and chores");
                };
                assert_eq!(
                    serde_json::to_value(task).unwrap(),
                    tree_cards[task.id.as_str()],
                    "patched card {} differs from the tree's card at production scale",
                    task.id
                );
            }
        }
        millis.sort_by(f64::total_cmp);
        let p50 = (samples * 50).div_ceil(100) - 1;
        let p95 = (samples * 95).div_ceil(100) - 1;
        println!(
            "scale runtime_patch cards_requested={count} cards_returned={returned} bytes={bytes} \
             samples={samples} patch_ms p50={:.3} p95={:.3}",
            millis[p50], millis[p95]
        );
    }
}

fn measure_deltas(db: &WorkDb, product: &str) {
    use boss_protocol::work_tree_delta::WorkTreeCache;
    let mut cache = WorkTreeCache::default();
    let tree = db.get_work_tree(product).unwrap();
    let snapshot = cache.update(&tree).unwrap();
    println!(
        "scale snapshot_update_bytes={}",
        serde_json::to_vec(&snapshot).unwrap().len()
    );
    for count in [1, 10, 100] {
        let conn = db.connect().unwrap();
        for task in tree.tasks.iter().take(count) {
            conn.execute("UPDATE tasks SET name = name || ' changed' WHERE id = ?1", [&task.id])
                .unwrap();
        }
        drop(conn);
        let start = Instant::now();
        let next = db.get_work_tree(product).unwrap();
        let update = cache.update(&next).unwrap();
        let bytes = serde_json::to_vec(&update).unwrap().len();
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        assert!(update.base_sequence.is_some());
        assert_eq!(update.changes["tasks"]["upserts"].as_array().unwrap().len(), count);
        assert!(bytes < serde_json::to_vec(&next).unwrap().len() / 10);
        println!("scale changed_records={count} delta_update_bytes={bytes} db_diff_serialize_ms={elapsed:.3}");
    }
}

// Optional authorized snapshot measurement. Copies into Bazel's private tmpdir;
// no source rows, identifiers, or descriptions are printed or checked in.
#[test]
fn work_tree_authorized_snapshot_measurement() {
    let Ok(source) = std::env::var("BOSS_BENCH_SNAPSHOT") else {
        return;
    };
    let (_dir, path) = disk_db_path("snapshot-scale");
    let source = Connection::open_with_flags(
        format!("file:{source}?immutable=1"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    source.execute("VACUUM INTO ?1", [path.to_str().unwrap()]).unwrap();
    drop(source);
    let db = WorkDb::open(path).unwrap();
    let product: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT product_id FROM tasks GROUP BY product_id ORDER BY count(*) DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    measure(&db, &product, None);
}
