use super::*;

#[test]
fn reentry_reports_both_call_sites_and_leaves_the_holder_usable() {
    let db = crate::WorkDb::open(":memory:".into()).unwrap();
    let clone = db.clone();
    let held_line = line!() + 1;
    let held = db.connect().unwrap();
    let requested_line = line!() + 1;
    let error = clone.connect().err().expect("same-thread re-entry must fail");
    let message = error.to_string();
    assert!(message.contains("connection re-entry"), "{message}");
    for line in [held_line, requested_line] {
        assert!(message.contains(&format!("{}:{line}:", file!())), "{message}");
    }
    assert_eq!(held.query_row("SELECT 42", [], |row| row.get::<_, i64>(0)).unwrap(), 42);
    drop(held);
    drop(clone.connect().expect("dropping the holder permits a new borrow"));
}

#[test]
fn independent_connections_can_be_held_and_released_out_of_order() {
    let first = crate::WorkDb::open(":memory:".into()).unwrap();
    let second = crate::WorkDb::open(":memory:".into()).unwrap();
    let first_guard = first.connect().unwrap();
    let second_guard = second.connect().unwrap();
    drop(first_guard);
    drop(first.connect().unwrap());
    assert!(second.connect().is_err(), "the other connection is still held");
    drop(second_guard);
    drop(second.connect().unwrap());
}

#[test]
fn another_thread_can_acquire_after_the_holder_releases() {
    let db = crate::WorkDb::open(":memory:".into()).unwrap();
    let held = db.connect().unwrap();
    std::thread::scope(|scope| {
        let (entered, ready) = std::sync::mpsc::channel();
        let db = &db;
        let other = scope.spawn(move || {
            assert!(matches!(db.conn.try_lock(), Err(TryLockError::WouldBlock)));
            entered.send(()).unwrap();
            drop(db.connect().expect("another thread is contention, not re-entry"));
        });
        ready.recv().unwrap();
        drop(held);
        other.join().unwrap();
    });
}

#[test]
fn unwinding_removes_ownership_without_hiding_mutex_poisoning() {
    let connection = Mutex::new(Connection::open_in_memory().unwrap());
    let metrics = crate::personas::new_metrics_registry();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = acquire(&connection, &metrics).unwrap();
        panic!("holder failed");
    }));
    assert!(result.is_err());
    let error = acquire(&connection, &metrics).err().expect("poisoned mutex");
    assert_eq!(error.to_string(), "work db connection lock poisoned");
    connection.clear_poison();
    drop(acquire(&connection, &metrics).unwrap());
}
