use super::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn committed_write_before_publication_cannot_join_the_old_tree() {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("tree.db")).unwrap();
    let product = db
        .create_product(boss_protocol::CreateProductInput::builder().name("Before").build())
        .unwrap();
    let loads = WorkTreeLoads::default();
    let revision = AtomicU64::new(0);
    let (started, running) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = StdMutex::new(gate);
    let (joining, registered) = mpsc::channel();
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            loads
                .flights
                .run(
                    product.id.clone(),
                    || loads.freshness(&db, &revision),
                    || {
                        let tree = db.reader().unwrap().get_work_tree(&product.id).unwrap();
                        started.send(()).unwrap();
                        gate.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok((0, tree))
                    },
                )
                .unwrap()
        });
        running.recv_timeout(Duration::from_secs(10)).unwrap();
        // An independent writer proves the fence does not rely on a callback
        // on the engine's own connection, or on publishing an invalidation.
        let writer = rusqlite::Connection::open(db.path()).unwrap();
        writer
            .execute("UPDATE products SET name = 'After' WHERE id = ?1", [&product.id])
            .unwrap();
        let second = scope.spawn(|| {
            loads
                .flights
                .run(
                    product.id.clone(),
                    || {
                        let stamp = loads.freshness(&db, &revision);
                        joining.send(()).unwrap();
                        stamp
                    },
                    || Ok((0, db.reader().unwrap().get_work_tree(&product.id).unwrap())),
                )
                .unwrap()
        });
        registered.recv_timeout(Duration::from_secs(10)).unwrap();
        release.send(()).unwrap();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_eq!(first.value.as_ref().unwrap().1.product.name, "Before");
        assert_eq!(second.value.as_ref().unwrap().1.product.name, "After");
        assert!(second.sequence > first.sequence);
        assert_eq!(revision.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn invalidation_without_a_database_write_changes_the_fence() {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("tree.db")).unwrap();
    let loads = WorkTreeLoads::default();
    let revision = AtomicU64::new(0);
    let before = loads.freshness(&db, &revision).unwrap();
    revision.fetch_add(1, Ordering::SeqCst);
    let after = loads.freshness(&db, &revision).unwrap();
    assert_ne!(before, after);
    assert_eq!(before.1, after.1);
}

#[test]
fn freshness_probe_and_tree_read_do_not_wait_for_an_uncommitted_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("tree.db")).unwrap();
    let product = db
        .create_product(boss_protocol::CreateProductInput::builder().name("Committed").build())
        .unwrap();
    let writer = rusqlite::Connection::open(db.path()).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE products SET name = 'Uncommitted'")
        .unwrap();
    let loaded = WorkTreeLoads::default()
        .load(
            &db,
            &AtomicU64::new(0),
            &crate::metrics::Registry::new(),
            &product.id,
            &mut crate::population_timing::PopulationTrace::disabled(),
        )
        .unwrap();
    assert_eq!(loaded.value.as_ref().unwrap().1.product.name, "Committed");
    writer.execute_batch("ROLLBACK").unwrap();
}
