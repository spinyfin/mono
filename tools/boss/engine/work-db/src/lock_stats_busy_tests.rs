use super::*;
use crate::connection_wait::acquire;
use rusqlite::Connection;

#[test]
fn result_boundary_preserves_success_and_non_busy_errors() {
    let stats = LockStats::default();
    assert_eq!(returned_with(&stats, Ok(42)).unwrap(), 42);
    let result: anyhow::Result<()> = Err(rusqlite::Error::QueryReturnedNoRows.into());
    let error = returned_with(&stats, result).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::QueryReturnedNoRows)
    ));
    assert!(error.downcast_ref::<CountedBusy>().is_none());
    assert!(stats.take_summary().is_none());
}

#[test]
fn busy_then_success_and_empty_guards_count_exactly_once() {
    assert_busy_count(true);
}

#[test]
fn busy_then_empty_guards_count_exactly_once() {
    assert_busy_count(false);
}

fn assert_busy_count(run_success: bool) {
    let stats = LockStats::default();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy.db");
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("CREATE TABLE t(x); BEGIN IMMEDIATE").unwrap();
    let connection = Mutex::new(Connection::open(&path).unwrap());
    let result: anyhow::Result<()> = (|| {
        let conn = acquire(&connection, &crate::personas::new_metrics_registry())?;
        conn.busy_handler(None)?;
        let result = conn.execute("INSERT INTO t VALUES (1)", []);
        // A successful statement overwrites connection state before guard drop.
        if run_success {
            conn.query_row("SELECT 1", [], |_| Ok(()))?;
        }
        result?;
        Ok(())
    })();
    let error = returned_with(&stats, result).unwrap_err();
    assert!(is_busy_error(error.downcast_ref().unwrap()));
    // Nested result boundaries must preserve the marker through context.
    let result: anyhow::Result<()> = Err(error.context("outer operation"));
    assert!(returned_with(&stats, result).is_err());
    for _ in 0..5 {
        drop(acquire(&connection, &crate::personas::new_metrics_registry()).unwrap());
    }
    assert_eq!(stats.take_summary().unwrap().busy, 1);
    assert!(stats.take_summary().is_none());
}

#[test]
fn deferred_snapshot_busy_survives_automatic_rollback() {
    let stats = LockStats::default();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.db");
    let mut reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES(1)")
        .unwrap();
    let writer = Connection::open(&path).unwrap();
    let result: anyhow::Result<()> = (|| {
        let tx = reader.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        tx.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?;
        writer.execute("INSERT INTO t VALUES(2)", [])?;
        tx.execute("INSERT INTO t VALUES(3)", [])?;
        tx.commit()?;
        Ok(())
    })();
    assert!(reader.is_autocommit(), "transaction must have rolled back");
    let error = returned_with(&stats, result).unwrap_err();
    let rusqlite::Error::SqliteFailure(code, _) = error.downcast_ref::<rusqlite::Error>().unwrap() else {
        panic!("expected SQLite failure: {error:#}");
    };
    assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_BUSY_SNAPSHOT);
    assert_eq!(stats.take_summary().unwrap().busy, 1);
}

#[test]
fn independent_reader_busy_uses_returned_error() {
    let stats = LockStats::default();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reader.db");
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES(1)")
        .unwrap();
    let reader = Connection::open(&path).unwrap();
    reader.busy_handler(None).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let result = reader.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0));
    assert!(returned_with(&stats, result.map_err(Into::into)).is_err());
    assert_eq!(stats.take_summary().unwrap().busy, 1);
}
