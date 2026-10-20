use super::*;
use crate::population_timing::PopulationTrace;
use crate::test_support::{create_test_product, open_db};
use crate::work::WorkDb;
use std::time::{Duration, Instant};

fn trace(product_id: &str) -> PopulationTrace {
    PopulationTrace::new(product_id.to_owned(), "req".to_owned(), None, Instant::now())
}

/// Take the write lock on an independent connection and hold it for `hold`.
fn hold_write_lock(db: &WorkDb, hold: Duration) -> std::thread::JoinHandle<()> {
    let holder = db.connect_new().unwrap();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        holder
            .execute_batch("BEGIN IMMEDIATE; UPDATE metadata SET value = value WHERE key = 'schema_version'")
            .unwrap();
        locked_tx.send(()).unwrap();
        std::thread::sleep(hold);
        holder.execute_batch("COMMIT").unwrap();
    });
    locked_rx.recv().unwrap();
    handle
}

/// Loads that do not wait on a lock are never shared: each request reads the
/// database itself.
#[test]
fn uncontended_loads_each_read_the_database() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let flights = WorkTreeFlights::default();
    for _ in 0..3 {
        let tree = flights
            .load(&product.id, || {
                db.get_work_tree_instrumented(&product.id, &mut trace(&product.id))
            })
            .unwrap();
        assert_eq!(tree.product.id, product.id);
    }
    assert_eq!((flights.loads(), flights.shared()), (3, 0));
}

/// Requests that arrive while a load is blocked on a real SQLite write lock
/// share that load: one database read, a tree for every waiter.
#[test]
fn requests_arriving_while_a_load_waits_for_the_lock_share_it() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let flights = Arc::new(WorkTreeFlights::default());
    let holder = hold_write_lock(&db, Duration::from_millis(1500));

    let leader = std::thread::spawn({
        let (flights, db, id) = (flights.clone(), db.clone(), product.id.clone());
        move || {
            flights.load(&id, || {
                // Taking the write lock waits behind the external holder.
                db.connect()?.transaction()?.commit()?;
                db.get_work_tree_instrumented(&id, &mut trace(&id))
            })
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flights
        .flights
        .lock()
        .unwrap()
        .get(&product.id)
        .is_some_and(|flight| flight.wait.is_waiting())
    {
        assert!(Instant::now() < deadline, "the leader never started waiting");
        std::thread::sleep(Duration::from_millis(10));
    }
    let followers: Vec<_> = (0..5)
        .map(|_| {
            std::thread::spawn({
                let (flights, id) = (flights.clone(), product.id.clone());
                move || flights.load(&id, || panic!("a waiter must not read the database"))
            })
        })
        .collect();
    while flights.shared() < 5 {
        assert!(Instant::now() < deadline, "waiters never joined the load");
        std::thread::sleep(Duration::from_millis(10));
    }
    holder.join().unwrap();
    let trees: Vec<_> = std::iter::once(leader)
        .chain(followers)
        .map(|handle| handle.join().unwrap().unwrap())
        .collect();
    assert_eq!(trees.len(), 6);
    assert!(trees.iter().all(|tree| tree.product.id == product.id));
    assert_eq!((flights.loads(), flights.shared()), (1, 5));
    assert!(flights.flights.lock().unwrap().is_empty());
}

/// A failing load fails its waiters too, and a panicking one releases them.
#[test]
fn a_failed_or_panicking_load_still_answers_its_waiters() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let flights = Arc::new(WorkTreeFlights::default());
    let holder = hold_write_lock(&db, Duration::from_millis(800));
    let leader = std::thread::spawn({
        let (flights, db, id) = (flights.clone(), db.clone(), product.id.clone());
        move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                flights.load(&id, || {
                    db.connect()?.transaction()?.commit()?;
                    panic!("load blew up")
                })
            }))
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flights
        .flights
        .lock()
        .unwrap()
        .get(&product.id)
        .is_some_and(|flight| flight.wait.is_waiting())
    {
        assert!(Instant::now() < deadline, "the leader never started waiting");
        std::thread::sleep(Duration::from_millis(10));
    }
    let waiter = std::thread::spawn({
        let (flights, id) = (flights.clone(), product.id.clone());
        move || flights.load(&id, || panic!("a waiter must not read the database"))
    });
    while flights.shared() < 1 {
        assert!(Instant::now() < deadline, "the waiter never joined");
        std::thread::sleep(Duration::from_millis(10));
    }
    holder.join().unwrap();
    assert!(leader.join().unwrap().is_err(), "the leader's panic propagates");
    let err = waiter.join().unwrap().expect_err("the waiter must be failed, not hung");
    assert!(err.to_string().contains("aborted"), "{err:#}");
    assert!(flights.flights.lock().unwrap().is_empty());
}
