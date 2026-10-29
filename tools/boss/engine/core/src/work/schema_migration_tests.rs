use super::super::{WorkDb, registry};
use super::*;

fn independent() -> [Migration; 2] {
    [
        Migration {
            id: "20261028_1200_alpha".into(),
            legacy_version: None,
            apply: |conn| {
                conn.execute_batch("CREATE TABLE alpha (value TEXT); INSERT INTO trace VALUES ('alpha')")?;
                Ok(())
            },
        },
        Migration {
            id: "20261028_1201_beta".into(),
            legacy_version: None,
            apply: |conn| {
                conn.execute_batch("CREATE TABLE beta (value TEXT); INSERT INTO trace VALUES ('beta')")?;
                Ok(())
            },
        },
    ]
}

fn trace(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT name FROM trace ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn independent_migrations_run_in_id_order_regardless_of_registry_order() {
    for reverse in [false, true] {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(CREATE_LEDGER).unwrap();
        conn.execute_batch("CREATE TABLE trace (name TEXT)").unwrap();
        let mut migrations = independent();
        if reverse {
            migrations.reverse();
        }
        for _ in 0..2 {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            apply(&tx, &migrations, None).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(trace(&conn), ["alpha", "beta"]);
        assert_eq!(applied(&conn).unwrap().len(), 2);
    }
}

#[test]
fn earlier_id_added_later_is_still_applied() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CREATE_LEDGER).unwrap();
    conn.execute_batch("CREATE TABLE trace (name TEXT)").unwrap();
    let migrations = independent();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    apply(&tx, &migrations[1..], None).unwrap();
    tx.commit().unwrap();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    apply(&tx, &migrations, None).unwrap();
    tx.commit().unwrap();
    assert_eq!(trace(&conn), ["beta", "alpha"]);
}

#[test]
fn bootstrap_last_integer_version_never_replays_legacy_migrations() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT); INSERT INTO metadata VALUES ('schema_version', '60')")
        .unwrap();
    let mut known: Vec<_> = registry::all()
        .into_iter()
        .filter(|m| m.legacy_version.is_some())
        .collect();
    for migration in &mut known {
        migration.apply = |_| panic!("an applied legacy migration was replayed");
    }
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    apply(&tx, &known, Some(60)).unwrap();
    tx.commit().unwrap();
    assert_eq!(applied(&conn).unwrap(), known.iter().map(|m| m.id.clone()).collect());
    let before: Vec<(String, String)> = conn
        .prepare("SELECT id, applied_at FROM schema_migrations ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    apply(&tx, &known, None).unwrap();
    tx.commit().unwrap();
    let after: Vec<(String, String)> = conn
        .prepare("SELECT id, applied_at FROM schema_migrations ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn unknown_id_refuses_engine_startup_before_applying_pending_migrations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.db");
    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "INSERT INTO schema_migrations VALUES ('20990101_0000_future', 'future');
         DELETE FROM schema_migrations WHERE id = '00000000_0000_v035_restore_reports';
         DROP TABLE execution_restore_reports;",
    )
    .unwrap();
    let before = applied(&conn).unwrap();
    drop(conn);
    drop(db);
    let error = WorkDb::open(path.clone())
        .err()
        .expect("unknown migration must reject startup");
    let message = format!("{error:#}");
    assert!(
        message.contains("20990101_0000_future") && message.contains("newer"),
        "{message}"
    );
    let conn = Connection::open(path).unwrap();
    assert_eq!(applied(&conn).unwrap(), before);
    assert!(!super::super::table_exists(&conn, "execution_restore_reports").unwrap());
}

#[test]
fn unknown_integer_version_is_rejected_without_bootstrap() {
    let db = WorkDb::open_in_memory().unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch("DROP TABLE schema_migrations; INSERT INTO metadata VALUES ('schema_version', '61')")
        .unwrap();
    drop(conn);
    let error = db.init().unwrap_err();
    assert!(format!("{error:#}").contains("newer"));
    let conn = db.connect().unwrap();
    assert!(!super::super::table_exists(&conn, "schema_migrations").unwrap());
    let version: String = conn
        .query_row("SELECT value FROM metadata WHERE key = 'schema_version'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, "61");
}

#[test]
fn failed_migration_rolls_back_schema_data_and_ledger_including_bootstrap() {
    for bootstrap in [false, true] {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT); INSERT INTO metadata VALUES ('schema_version', '0'); CREATE TABLE trace (name TEXT)").unwrap();
        if !bootstrap {
            conn.execute_batch(CREATE_LEDGER).unwrap();
        }
        let mut known = independent().into_iter().collect::<Vec<_>>();
        known.push(Migration {
            id: "20261028_1202_failure".into(),
            legacy_version: None,
            apply: |conn| {
                conn.execute_batch("INSERT INTO trace VALUES ('failure'); CREATE TABLE partial (id INTEGER)")?;
                anyhow::bail!("intentional migration failure")
            },
        });
        {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            let error = apply(&tx, &known, bootstrap.then_some(0)).unwrap_err();
            assert!(format!("{error:#}").contains("20261028_1202_failure"));
        }
        assert!(trace(&conn).is_empty());
        for table in ["alpha", "beta", "partial"] {
            assert!(!super::super::table_exists(&conn, table).unwrap());
        }
        if bootstrap {
            assert!(!super::super::table_exists(&conn, "schema_migrations").unwrap());
            let version: String = conn
                .query_row("SELECT value FROM metadata WHERE key = 'schema_version'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(version, "0");
        } else {
            assert!(applied(&conn).unwrap().is_empty());
        }
    }
}

#[test]
fn duplicate_ids_are_rejected_before_any_changes() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CREATE_LEDGER).unwrap();
    conn.execute_batch("CREATE TABLE trace (name TEXT)").unwrap();
    let mut known = independent();
    known[1].id = known[0].id.clone();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    assert!(apply(&tx, &known, None).unwrap_err().to_string().contains("duplicate"));
    assert!(trace(&tx).is_empty());
    assert!(applied(&tx).unwrap().is_empty());
}

#[test]
fn historical_ids_preserve_integer_order() {
    let mut known = registry::all();
    known.sort_by(|a, b| a.id.cmp(&b.id));
    let versions: Vec<_> = known.iter().filter_map(|m| m.legacy_version).collect();
    assert_eq!(versions, (1..=60).collect::<Vec<_>>());
}

#[test]
fn completion_capture_migrates_existing_named_schema_and_survives_reopen() {
    let db = WorkDb::open_in_memory().unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "DELETE FROM schema_migrations WHERE id = '20261028_1900_pr_head_after_capture';
         ALTER TABLE work_executions DROP COLUMN pr_head_after_capture;",
    )
    .unwrap();
    drop(conn);
    db.init().unwrap();
    let conn = db.connect().unwrap();
    assert!(super::super::table_has_column(&conn, "work_executions", "pr_head_after_capture").unwrap());
    assert!(applied(&conn).unwrap().contains("20261028_1900_pr_head_after_capture"));
    conn.execute_batch(
        "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at, pr_head_after_capture)
         VALUES ('capture-execution', 'work', 'chore_implementation', 'completed', 'repo', '2026-01-01', 'recorded')",
    )
    .unwrap();
    drop(conn);
    db.init().unwrap();
    let conn = db.connect().unwrap();
    let capture: String = conn
        .query_row(
            "SELECT pr_head_after_capture FROM work_executions WHERE id = 'capture-execution'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(capture, "recorded");
}
