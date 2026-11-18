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
            &boss_metrics::Registry::new(),
            &product.id,
            &mut boss_metrics::population_timing::PopulationTrace::disabled(),
        )
        .unwrap();
    assert_eq!(loaded.value.as_ref().unwrap().1.product.name, "Committed");
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn projection_retention_evicts_the_least_recently_read_product() {
    let loads = WorkTreeLoads::default();
    let hot = loads.projection("hot");
    let cold = loads.projection("cold");
    loads.projection("third");
    loads.projection("fourth");
    assert!(Arc::ptr_eq(&hot, &loads.projection("hot")));
    loads.projection("fifth");
    assert!(Arc::ptr_eq(&hot, &loads.projection("hot")));
    assert!(!Arc::ptr_eq(&cold, &loads.projection("cold")));
}

type SharedTree = Arc<boss_single_flight::Loaded<(u64, boss_protocol::WorkTree)>>;

/// A normal uncommitted WAL writer does not block reads, but an external
/// connection in exclusive locking mode does, including the freshness probe.
fn share_an_exclusively_locked_load(fail: bool) -> Vec<SharedTree> {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("tree.db")).unwrap();
    let product = db
        .create_product(boss_protocol::CreateProductInput::builder().name("Committed").build())
        .unwrap();
    if fail {
        db.connect().unwrap().execute_batch("DROP TABLE products").unwrap();
    }
    // Release the initialized writer's WAL locks. Reader construction is lazy:
    // these connections do not touch SQLite pages until load's freshness probe.
    let db = {
        let reader = db.reader().unwrap();
        drop(db);
        reader
    };
    let (probed, probes) = mpsc::channel::<()>();
    let loads = WorkTreeLoads::default();
    let revision = AtomicU64::new(0);
    let metrics = boss_metrics::Registry::new();
    let slot = loads.projection(&product.id);
    let held = slot.lock().unwrap();
    // Observe real freshness probes under the single-flight registration lock.
    let reader = db.reader().unwrap();
    unsafe extern "C" fn completed(
        _: std::ffi::c_uint,
        context: *mut std::ffi::c_void,
        _: *mut std::ffi::c_void,
        _: *mut std::ffi::c_void,
    ) -> std::ffi::c_int {
        // SAFETY: the sender lives until the probe connection is dropped below.
        let sender = unsafe { &*context.cast::<mpsc::Sender<()>>() };
        let _ = sender.send(());
        0
    }
    // SAFETY: the callback only sends a notification, never calls SQLite,
    // and its sender outlives the connection and every loading thread.
    unsafe {
        rusqlite::ffi::sqlite3_trace_v2(
            reader.connect().unwrap().handle(),
            rusqlite::ffi::SQLITE_TRACE_PROFILE,
            Some(completed),
            std::ptr::from_ref(&probed).cast_mut().cast(),
        );
    }
    *loads.probe.lock().unwrap() = Some(reader);
    // Open the same independent readers dispatch supplies before taking the
    // lock, so contention happens inside load(), not during connection setup.
    let readers: Vec<_> = (0..6).map(|_| db.reader().unwrap()).collect();
    let holder = rusqlite::Connection::open(db.path()).unwrap();
    holder
        .execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE")
        .unwrap();
    if !fail {
        holder.execute("UPDATE products SET name = 'After'", []).unwrap();
    }
    let waits: Vec<_> = (0..6)
        .map(|_| Arc::new(boss_work_db::contention::ReadWait::default()))
        .collect();
    let entered = std::sync::Barrier::new(7);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = readers
            .iter()
            .zip(&waits)
            .map(|(reader, wait)| {
                let (loads, revision, metrics, product, entered) = (&loads, &revision, &metrics, &product, &entered);
                scope.spawn(move || {
                    entered.wait();
                    boss_work_db::contention::READ_WAIT.sync_scope(wait.clone(), || {
                        loads
                            .load(
                                reader,
                                revision,
                                metrics,
                                &product.id,
                                &mut boss_metrics::population_timing::PopulationTrace::disabled(),
                            )
                            .unwrap()
                    })
                })
            })
            .collect();
        entered.wait();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let blocked = loop {
            if waits.iter().any(|wait| wait.is_waiting()) {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        holder.execute_batch("COMMIT").unwrap();
        // Exclusive mode retains its lock until the connection closes.
        drop(holder);
        // The projection mutex is only a test publication gate: keep the
        // owner paused until every caller has passed its real freshness probe.
        // Six initial probes plus the owner's start probe complete while
        // the projection remains locked.
        let registered = (0..7).try_for_each(|_| probes.recv_timeout(Duration::from_secs(10)));
        drop(held);
        assert!(blocked, "the production load never waited on the external SQLite lock");
        registered.expect("all production loads must register before publication");
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    });
    drop(loads.probe.lock().unwrap().take());
    results
}

#[test]
fn production_load_shares_after_an_external_exclusive_lock() {
    let results = share_an_exclusively_locked_load(false);
    for result in &results {
        assert!(Arc::ptr_eq(result, &results[0]));
        assert_eq!(result.value.as_ref().unwrap().1.product.name, "After");
    }
}

#[test]
fn production_failed_load_after_an_exclusive_lock_answers_every_waiter() {
    let results = share_an_exclusively_locked_load(true);
    for result in &results {
        assert!(Arc::ptr_eq(result, &results[0]));
        assert!(result.value.as_ref().unwrap_err().contains("products"));
    }
}
