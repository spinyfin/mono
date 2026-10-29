use super::*;
use std::sync::{Arc, Barrier};
use std::time::Duration;

#[test]
fn attention_reconcile_releases_connection_between_kinds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing_subscriber::prelude::*;

    struct CheckUnlocked {
        db: WorkDb,
        events: Arc<AtomicUsize>,
        timing_events: Arc<AtomicUsize>,
    }
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CheckUnlocked {
        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            if event.metadata().target().ends_with("work::attention_reconcile") {
                assert!(
                    self.db.conn.try_lock().is_ok(),
                    "connection held between attention kinds"
                );
                if event.metadata().fields().field("max_batch_hold_ms").is_some() {
                    self.timing_events.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.events.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    let db = WorkDb::open(temp_db_path("attention-kind-holds")).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute_batch(
            "INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at)
             VALUES ('resumed','work','chore_implementation','running','repo','2');
             INSERT INTO work_runs(id,execution_id,agent_id,status,created_at,started_at)
             VALUES ('run','resumed','worker','running','2','2')",
        )
        .unwrap();
        for lifecycle in crate::attention_lifecycle::automatically_cleared()
            .filter(|entry| matches!(entry.cleared_by, crate::attention_lifecycle::ClearedBy::WorkResumed))
            .take(2)
        {
            conn.execute(
                "INSERT INTO work_attention_items(id,work_item_id,kind,status,title,body_markdown,created_at)
                 VALUES (?1,'work',?1,'open','fixture','','1')",
                [lifecycle.kind],
            )
            .unwrap();
        }
    }
    let events = Arc::new(AtomicUsize::new(0));
    let timing_events = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(CheckUnlocked {
        db: db.clone(),
        events: events.clone(),
        timing_events: timing_events.clone(),
    });
    tracing::subscriber::with_default(subscriber, || {
        assert_eq!(db.reconcile_stale_attention_signals().unwrap().attentions_resolved, 2);
        assert!(db.reconcile_stale_attention_signals().unwrap().is_empty());
    });
    assert_eq!(events.load(Ordering::Relaxed), 2);
    assert_eq!(
        timing_events.load(Ordering::Relaxed),
        2,
        "one batch timing per pass, including the no-op pass"
    );
}

#[test]
fn narrow_workspace_scan_filters_and_keeps_the_first_lease() {
    let db = WorkDb::open(temp_db_path("workspace-scan")).unwrap();
    db.connect().unwrap().execute_batch(
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<10000)
         INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at,cube_lease_id,workspace_path)
         SELECT printf('live-%05d',i),'fixture','chore_implementation','running','repo','2','lease','/later' FROM n;
         INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at,cube_lease_id,workspace_path)
         VALUES ('first','fixture','chore_implementation','running','repo','1','lease','/first'),
                ('missing-path','fixture','chore_implementation','running','repo','0','lease',NULL),
                ('unleased','fixture','chore_implementation','running','unleased','0',NULL,'/skip'),
                ('terminal','fixture','chore_implementation','completed','terminal','0','lease','/skip')",
    ).unwrap();
    assert_eq!(
        db.list_in_flight_repo_workspaces().unwrap(),
        HashMap::from([("repo".into(), "/first".into())])
    );
}

fn project(db: &WorkDb, product_id: &str) -> Project {
    let project = db
        .create_project(CreateProjectInput {
            product_id: product_id.into(),
            name: "Contention fixture".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    db.set_project_design_doc(set_design_doc_input(&project.id, "docs/design.md"))
        .unwrap()
}

#[test]
fn in_flight_plan_avoids_terminal_history_and_sort() {
    let db = WorkDb::open(temp_db_path("in-flight-plan")).unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<50000)
        INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at)
        SELECT 'history-'||i,'fixture','chore_implementation','completed','repo',CAST(i AS TEXT) FROM n;
        INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at,cube_lease_id,workspace_path)
        VALUES ('live','fixture','chore_implementation','running','repo','50001','lease','/tmp/workspace')",
    )
    .unwrap();
    let sql = "SELECT repo_remote_url, workspace_path FROM work_executions
        WHERE status NOT IN ('completed','failed','abandoned','cancelled','orphaned')
          AND cube_lease_id IS NOT NULL AND workspace_path IS NOT NULL ORDER BY created_at,id";
    let plan = |sql: &str| -> Vec<String> {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let indexed = plan(sql);
    assert!(
        indexed
            .iter()
            .any(|line| line.contains("work_executions_in_flight_idx")),
        "{indexed:?}"
    );
    assert!(!indexed.iter().any(|line| line.contains("TEMP B-TREE")), "{indexed:?}");
    // Measure the original plan against precisely the same synthetic history.
    conn.execute_batch("DROP INDEX work_executions_in_flight_idx").unwrap();
    let original = plan(sql);
    assert!(
        original.iter().any(|line| line.contains("SCAN work_executions")),
        "{original:?}"
    );
    assert!(original.iter().any(|line| line.contains("TEMP B-TREE")), "{original:?}");
    println!("synthetic execution rows=50001 original={original:?} indexed={indexed:?}");
    drop(conn);
    assert_eq!(
        db.list_in_flight_repo_workspaces().unwrap(),
        HashMap::from([("repo".into(), "/tmp/workspace".into())])
    );
    assert_eq!(db.list_in_flight_executions().unwrap().len(), 1);
}

#[test]
fn sixty_one_concurrent_resolvers_allow_writes_and_release_before_workspace_lookup() {
    let (_dir, path) = disk_db_path("design-doc-contention");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = project(&db, &product.id);
    let index_sql: String = {
        let conn = db.connect().unwrap();
        conn.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<50000)
            INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at,last_error)
            SELECT 'history-'||i,'fixture','chore_implementation','completed','repo',CAST(i AS TEXT),printf('%02048d',0) FROM n").unwrap();
        let sql = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='work_executions_in_flight_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute_batch("DROP INDEX work_executions_in_flight_idx").unwrap();
        sql
    };
    // Reproduce the original 61-way full-row scan on a synthetic history.
    // Report timing rather than asserting a machine-dependent wall-clock bound.
    let old_barrier = Arc::new(Barrier::new(61));
    let mut old_durations = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..61 {
            let reader = WorkDb::open_existing(path.clone()).unwrap();
            let barrier = old_barrier.clone();
            handles.push(scope.spawn(move || {
                barrier.wait();
                let start = std::time::Instant::now();
                assert!(reader.list_in_flight_executions().unwrap().is_empty());
                start.elapsed()
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    old_durations.sort();
    db.connect().unwrap().execute_batch(&index_sql).unwrap();
    println!(
        "synthetic history rows=50000 payload_bytes_per_row=2048 fanout=61 old_scan_median_ms={} old_scan_max_ms={}",
        old_durations[30].as_millis(),
        old_durations[60].as_millis()
    );
    let barrier = Arc::new(Barrier::new(62));
    let improved_start = std::time::Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..61 {
            let reader = WorkDb::open_existing(path.clone()).unwrap();
            let barrier = barrier.clone();
            let project_id = project.id.clone();
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..5 {
                    let paths = reader.list_in_flight_repo_workspaces().unwrap();
                    reader
                        .resolve_project_design_doc(&project_id, |repo| {
                            // This succeeds only when the resolver released its guard.
                            assert!(reader.conn.try_lock().is_ok());
                            std::thread::sleep(Duration::from_millis(2));
                            paths.get(repo).cloned()
                        })
                        .unwrap();
                }
            });
        }
        barrier.wait();
        let conn = db.connect().unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        for n in 0..100 {
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES('contention-test',?1)",
                [n.to_string()],
            )
            .unwrap();
        }
    });
    println!(
        "indexed 61 readers x 5 resolutions plus 100 writes elapsed_ms={}",
        improved_start.elapsed().as_millis()
    );
}

#[test]
fn open_kind_probe_uses_partial_index_over_large_resolved_history() {
    let (_dir, path) = disk_db_path("attention-open-kind-plan");
    let db = WorkDb::open(path).unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<20000)
         INSERT INTO work_attention_items(id,work_item_id,kind,status,title,body_markdown,created_at)
         SELECT 'a-'||i,'item-'||i,'k'||(i%5),'resolved','t','b',CAST(i AS TEXT) FROM n",
    )
    .unwrap();
    let plan: Vec<String> = conn
        .prepare(
            "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM work_attention_items WHERE status = 'open' AND kind = ?1)",
        )
        .unwrap()
        .query_map(["k1"], |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| line.contains("work_attention_items_open_kind_idx")),
        "{plan:?}"
    );
    assert!(
        !plan.iter().any(|line| line.contains("SCAN work_attention_items")),
        "{plan:?}"
    );
}

// The attention reconcile skips a row or selection that the per-statement
// deadline really interrupted, and only that.
use crate::work::attention_reconcile::{skip_interrupted, skip_interrupted_selection};
use crate::work::db_diagnostics::QueryDeadline;

/// Run a statement long enough for SQLite's progress handler to see an
/// already-expired deadline, and return the error it actually produced.
fn genuinely_interrupted(db: &WorkDb) -> anyhow::Error {
    let conn = db.connect().unwrap();
    let _deadline = QueryDeadline::after(Duration::ZERO);
    conn.prepare(
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 100000000) SELECT count(*) FROM n",
    )
    .unwrap()
    .query_row([], |row| row.get::<_, i64>(0))
    .map_err(anyhow::Error::from)
    .expect_err("an expired deadline interrupts a long statement")
}

#[test]
fn a_deadline_interrupted_statement_is_skipped_not_fatal() {
    let db = WorkDb::open(temp_db_path("attention-real-interrupt")).unwrap();
    assert_eq!(skip_interrupted("row", Err(genuinely_interrupted(&db))).unwrap(), 0);
    assert!(
        skip_interrupted_selection("kind", Err(genuinely_interrupted(&db)))
            .unwrap()
            .is_none(),
        "an interrupted selection ends that scan"
    );
    // The interruption ended with the deadline; the connection is still usable.
    let one: i64 = db
        .connect()
        .unwrap()
        .query_row("SELECT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(one, 1);
}

#[test]
fn other_errors_and_successes_pass_through() {
    assert!(skip_interrupted("row", Err(anyhow::anyhow!("disk I/O error"))).is_err());
    assert!(skip_interrupted_selection("kind", Err(anyhow::anyhow!("disk I/O error"))).is_err());
    assert_eq!(skip_interrupted("row", Ok(3)).unwrap(), 3);
    assert_eq!(
        skip_interrupted_selection("kind", Ok(vec!["a".to_owned()])).unwrap(),
        Some(vec!["a".to_owned()])
    );
}
