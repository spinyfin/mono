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

/// Start one load that blocks on a real external SQLite write lock, join
/// five more requests for the same product while it is blocked, then commit
/// the external write. `after_lock` is the rest of the leader's load once it
/// holds the lock. Returns every request's result (leader first) and how
/// many loads ran.
fn share_a_load_blocked_on_the_write_lock(
    after_lock: impl FnOnce(&WorkDb, &str) -> Result<(u64, boss_protocol::WorkTree), String> + Send,
) -> (Vec<SharedTree>, usize) {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("tree.db")).unwrap();
    let product = db
        .create_product(boss_protocol::CreateProductInput::builder().name("Before").build())
        .unwrap();
    let loads = WorkTreeLoads::default();
    let revision = AtomicU64::new(0);
    let ran = AtomicU64::new(0);
    // Another process's write transaction: it holds the write lock, and its
    // change is invisible to every other connection until it commits.
    let holder = rusqlite::Connection::open(db.path()).unwrap();
    holder
        .execute_batch("BEGIN IMMEDIATE; UPDATE products SET name = 'After'")
        .unwrap();
    let (started, running) = mpsc::channel();
    let (joining, registered) = mpsc::channel();
    let results = std::thread::scope(|scope| {
        let leader = scope.spawn(|| {
            loads
                .flights
                .run(
                    product.id.clone(),
                    || loads.freshness(&db, &revision),
                    || {
                        ran.fetch_add(1, Ordering::SeqCst);
                        started.send(()).unwrap();
                        // The engine's own connection waits here for the
                        // external holder: contention is absorbed, not
                        // returned.
                        let mut conn = db.connect().map_err(|err| err.to_string())?;
                        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                            .and_then(|tx| tx.commit())
                            .map_err(|err| err.to_string())?;
                        drop(conn);
                        after_lock(&db, &product.id)
                    },
                )
                .unwrap()
        });
        running.recv_timeout(Duration::from_secs(10)).unwrap();
        let followers: Vec<_> = (0..5)
            .map(|_| {
                let joining = joining.clone();
                let (loads, db, revision, ran, product) = (&loads, &db, &revision, &ran, &product);
                scope.spawn(move || {
                    loads
                        .flights
                        .run(
                            product.id.clone(),
                            || {
                                let stamp = loads.freshness(db, revision);
                                joining.send(()).unwrap();
                                stamp
                            },
                            || {
                                ran.fetch_add(1, Ordering::SeqCst);
                                Err("a request that arrived during the lock wait must not load".into())
                            },
                        )
                        .unwrap()
                })
            })
            .collect();
        // Each follower's stamp is taken, and its join decided, under the
        // flight lock the leader needs to publish; once all five have probed,
        // none can miss the load.
        for _ in 0..5 {
            registered.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        assert!(
            !leader.is_finished(),
            "the load must still be blocked on the write lock"
        );
        holder.execute_batch("COMMIT").unwrap();
        std::iter::once(leader)
            .chain(followers)
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    (results, ran.load(Ordering::SeqCst) as usize)
}

/// Requests that arrive while a load is blocked on a real external SQLite
/// write lock share that one load: one database read, the same tree for
/// every request, and that tree reflects the write they all waited behind.
#[test]
fn requests_arriving_while_a_load_waits_for_the_write_lock_share_it() {
    let (results, ran) = share_a_load_blocked_on_the_write_lock(|db, product| {
        let tree = db
            .reader()
            .and_then(|reader| reader.get_work_tree(product))
            .map_err(|err| err.to_string())?;
        Ok((0, tree))
    });
    assert_eq!(ran, 1, "one load must serve every request");
    assert_eq!(results.len(), 6);
    for result in &results {
        assert!(Arc::ptr_eq(result, &results[0]));
        assert_eq!(result.value.as_ref().unwrap().1.product.name, "After");
    }
}

/// A load that fails, or panics, after waiting for the lock still answers
/// every request that joined it: none is left waiting.
#[test]
fn a_failed_or_panicking_load_blocked_on_the_write_lock_answers_every_waiter() {
    let (results, ran) = share_a_load_blocked_on_the_write_lock(|_, _| Err("tree load failed".into()));
    assert_eq!(ran, 1);
    assert_eq!(results.len(), 6);
    for result in &results {
        assert_eq!(result.value.as_ref().unwrap_err(), "tree load failed");
    }

    let (results, ran) = share_a_load_blocked_on_the_write_lock(|_, _| panic!("tree load blew up"));
    assert_eq!(ran, 1);
    assert_eq!(results.len(), 6);
    for result in &results {
        assert_eq!(result.value.as_ref().unwrap_err(), "shared load panicked");
    }
}
