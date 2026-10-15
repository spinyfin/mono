use super::*;
use std::sync::{Arc, Barrier};

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
fn board_snapshot_is_readable_while_another_connection_owns_the_writer() {
    let (_dir, path) = disk_db_path("board-read-snapshot");
    let writer = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&writer);
    let reader = WorkDb::open_existing(path).unwrap();
    reader.connect().unwrap().busy_timeout(Duration::ZERO).unwrap();
    let mut conn = writer.connect().unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute("UPDATE products SET name='uncommitted' WHERE id=?1", [&product.id])
        .unwrap();
    // The old read-only board transaction reserved the writer and failed here.
    let error = reader.connect().unwrap().execute_batch("BEGIN IMMEDIATE").unwrap_err();
    assert_eq!(error.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy));
    assert_eq!(reader.get_work_tree(&product.id).unwrap().product.name, product.name);
    tx.rollback().unwrap();
}

#[test]
fn database_spawn_failures_do_not_count_or_reset_genuine_worker_churn() {
    let db = WorkDb::open(temp_db_path("database-spawn-failure")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Churn classification");
    let genuine = create_ready_chore_execution(&db, chore.id.clone());
    db.record_pre_start_failure(&genuine.id, "worker", None, "worker refused to start", &[])
        .unwrap();
    for _ in 0..6 {
        let execution = create_ready_chore_execution(&db, chore.id.clone());
        let (_, run, outcome) = db
            .record_pre_start_database_failure(&execution.id, "worker", None, "database is locked")
            .unwrap();
        assert!(run.is_some());
        assert!(matches!(outcome, PreStartFailureOutcome::PermanentFail));
    }
    assert_eq!(db.count_recent_terminal_executions(&chore.id, 0, None).unwrap(), 1);
    assert_eq!(
        db.list_recent_terminal_execution_ids(&chore.id, 0, None).unwrap(),
        vec![genuine.id.clone()]
    );
    assert_eq!(
        db.list_consecutive_unproductive_terminal_execution_ids(&chore.id)
            .unwrap(),
        vec![genuine.id]
    );
}

#[test]
fn migration_classifies_only_historical_database_failures_before_worker_start() {
    let (_dir, path) = disk_db_path("database-failure-migration");
    {
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch("DROP INDEX work_executions_in_flight_idx;
            ALTER TABLE work_executions DROP COLUMN engine_database_failure;
            UPDATE metadata SET value='42' WHERE key='schema_version';
            INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at,pre_start_failure_count,last_error,started_at)
            VALUES ('prestart-busy','fixture','chore_implementation','failed','repo','1',1,'database is locked',NULL),
                   ('worker-busy','fixture','chore_implementation','failed','repo','2',1,'database is locked','1'),
                   ('prestart-other','fixture','chore_implementation','failed','repo','3',1,'missing binary',NULL)").unwrap();
    }
    let db = WorkDb::open(path).unwrap();
    let conn = db.connect().unwrap();
    let rows: Vec<(String, bool)> = conn
        .prepare("SELECT id, engine_database_failure FROM work_executions ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("prestart-busy".into(), true),
            ("prestart-other".into(), false),
            ("worker-busy".into(), false)
        ]
    );
}
