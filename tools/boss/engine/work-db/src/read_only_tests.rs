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

/// File-backed WAL connections reproduce production: every board must finish
/// while a writer owns the reservation, including the writable query opener.
#[test]
fn concurrent_boards_and_review_accounting_do_not_take_the_writer_lock() {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("state.db")).unwrap();
    let products: Vec<_> = (0..16)
        .map(|i| {
            let product = crate::test_support::create_test_product_named(&db, &format!("product-{i}"));
            for j in 0..8 {
                crate::test_support::create_test_chore(&db, &product.id, format!("fixture-{i}-{j}"));
            }
            product
        })
        .collect();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET name = 'before'", [])
        .unwrap();
    let writer = db.connect_new().unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE tasks SET name = 'uncommitted'")
        .unwrap();
    std::thread::scope(|scope| {
        for product in &products {
            let db = &db;
            scope.spawn(move || {
                for reader in [db.reader().unwrap(), db.query_connection().unwrap()] {
                    let tree = reader.get_work_tree(&product.id).unwrap();
                    assert_eq!(tree.chores.len(), 8);
                    assert!(tree.chores.iter().all(|task| task.name == "before"));
                    assert!(
                        reader
                            .can_admit_review_batch(boss_protocol::ReviewBatchPhase::PreMerge)
                            .unwrap()
                    );
                    assert_eq!(reader.review_pool_reserved_units().unwrap(), (0, 0));
                }
            });
        }
    });
    writer.execute_batch("COMMIT").unwrap();
    assert!(
        db.reader()
            .unwrap()
            .get_work_tree(&products[0].id)
            .unwrap()
            .chores
            .iter()
            .all(|task| task.name == "uncommitted")
    );
}

#[test]
fn board_projections_share_one_snapshot_while_a_writer_commits() {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("state.db")).unwrap();
    let product = crate::test_support::create_test_product_named(&db, "before");
    for i in 0..64 {
        crate::test_support::create_test_chore(&db, &product.id, format!("fixture-{i}"));
    }
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET name = 'before'", [])
        .unwrap();
    let reader = db.reader().unwrap();
    let writer = db.connect_new().unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut first = true;
    // Preparing the projects projection follows the completed product read,
    // so the snapshot is established before the writer is released.
    reader
        .connect()
        .unwrap()
        .authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
            if first
                && matches!(
                    ctx.action,
                    rusqlite::hooks::AuthAction::Read {
                        table_name: "projects",
                        ..
                    }
                )
            {
                first = false;
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            rusqlite::hooks::Authorization::Allow
        }));
    let handle = std::thread::spawn(move || reader.get_work_tree(&product.id).unwrap());
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE products SET name = 'after'; UPDATE tasks SET name = 'after'; COMMIT")
        .unwrap();
    release_tx.send(()).unwrap();
    let tree = handle.join().unwrap();
    assert_eq!(tree.product.name, "before");
    assert_eq!(tree.chores.len(), 64);
    assert!(tree.chores.iter().all(|task| task.name == "before"));
}
