use super::*;

#[test]
fn inspection_never_migrates_or_waits_for_the_wal_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = WorkDb::open(path.clone()).unwrap();
    let writer = db.connect_new().unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE metadata SET value = value")
        .unwrap();

    // Reproduce the old CLI startup path with a short test-only timeout:
    // even an up-to-date schema requires the writer lock during init.
    db.connect().unwrap().busy_timeout(Duration::from_millis(30)).unwrap();
    let error = db.init().unwrap_err();
    assert!(format!("{error:#}").contains("locked"), "{error:#}");

    let started = std::time::Instant::now();
    let existing = WorkDb::open_existing(path.clone()).unwrap();
    assert!(existing.review_batches_for_cycle_root("absent").unwrap().is_empty());
    let reader = WorkDb::open_read_only(path).unwrap();
    assert!(reader.review_batches_for_cycle_root("absent").unwrap().is_empty());
    assert!(started.elapsed() < Duration::from_secs(1));
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(reader.set_metadata("must_not_write", "value").is_err());
}

#[test]
fn inspection_does_not_create_missing_databases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.db");
    assert!(WorkDb::open_read_only(path.clone()).is_err());
    assert!(WorkDb::open_existing(path.clone()).is_err());
    assert!(!path.exists());
}

#[test]
fn admitted_queries_do_not_wait_for_the_engine_connection_mutex() {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("state.db")).unwrap();
    let held = db.connect().unwrap();
    let reader = db.query_connection().unwrap();
    assert!(reader.review_batches_for_cycle_root("absent").unwrap().is_empty());
    // Refresh-capable Get handlers retain normal write semantics, but the
    // connection is independent of the mutex held above.
    reader.set_metadata("refresh", "value").unwrap();
    drop(held);
}
