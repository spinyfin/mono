use super::*;
use crate::test_support::{create_test_product, log_capture, open_db};
use crate::work::{CreateChoreInput, DuplicateTaskError, WorkDb};
use rusqlite::ffi;
use std::sync::mpsc;
use std::thread;

/// Log lines about SQLite lock waits emitted since `from`.
fn wait_lines(buffer: &log_capture::SharedBuffer, from: usize, needle: &str) -> Vec<String> {
    String::from_utf8_lossy(&buffer.lock()[from..])
        .lines()
        .filter(|line| line.contains(needle))
        .map(str::to_owned)
        .collect()
}

/// Take the write lock on an independent connection to `db` and hold it
/// for `hold`. Returns once the lock is held; the thread releases it on its
/// own, so the caller's next write genuinely has to wait.
fn hold_write_lock(db: &WorkDb, hold: Duration) -> thread::JoinHandle<()> {
    let holder = db.connect_new().unwrap();
    let (locked_tx, locked_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        holder
            .execute_batch("BEGIN IMMEDIATE; UPDATE metadata SET value = value WHERE key = 'schema_version'")
            .unwrap();
        locked_tx.send(()).unwrap();
        thread::sleep(hold);
        holder.execute_batch("COMMIT").unwrap();
    });
    locked_rx.recv().unwrap();
    handle
}

fn sqlite_error(code: i32) -> anyhow::Error {
    anyhow::Error::new(rusqlite::Error::SqliteFailure(ffi::Error::new(code), None)).context("wrapped")
}

#[test]
fn busy_and_locked_codes_are_contention_and_others_are_not() {
    for code in [
        ffi::SQLITE_BUSY,
        ffi::SQLITE_BUSY_RECOVERY,
        ffi::SQLITE_BUSY_SNAPSHOT,
        ffi::SQLITE_BUSY_TIMEOUT,
        ffi::SQLITE_LOCKED,
        ffi::SQLITE_LOCKED_SHAREDCACHE,
    ] {
        let err = sqlite_error(code);
        let contention = classify(&err).unwrap_or_else(|| panic!("{code} must classify as contention"));
        assert_eq!(contention.extended_code, code);
    }
    for code in [
        ffi::SQLITE_CONSTRAINT,
        ffi::SQLITE_IOERR,
        ffi::SQLITE_CORRUPT,
        ffi::SQLITE_READONLY,
    ] {
        assert!(classify(&sqlite_error(code)).is_none(), "{code} is a real failure");
    }
    assert!(classify(&anyhow::anyhow!("not a database error")).is_none());

    // A real failure from a live connection: the message stays intact and
    // is not contention.
    let (_dir, db) = open_db();
    let err = db
        .connect()
        .unwrap()
        .execute("INSERT INTO metadata (key, value) VALUES ('schema_version', 'dup')", [])
        .map_err(anyhow::Error::new)
        .unwrap_err();
    assert!(classify(&err).is_none(), "{err:#}");
    assert!(format!("{err:#}").contains("UNIQUE"), "{err:#}");
}

/// A write issued while another connection holds the write lock waits,
/// commits exactly once, and the wait is logged with caller, request kind,
/// attempt count and total wait. The duplicate-create guard still fires
/// for the second create instead of a lock error.
#[test]
fn write_under_external_write_lock_commits_once_and_logs_the_wait() {
    let buffer = log_capture::install();
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let from = buffer.lock().len();

    let holder = hold_write_lock(&db, Duration::from_millis(300));
    let started = Instant::now();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Contended create")
                .build(),
        )
        .expect("write under contention must succeed");
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "the write did not wait"
    );
    holder.join().unwrap();

    let count: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE name = 'Contended create'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "a retried write must be applied exactly once");
    assert_eq!(chore.name, "Contended create");

    let lines = wait_lines(&buffer, from, "SQLite lock wait cleared");
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(
        line.contains("caller=") && line.contains("create_entities.rs"),
        "{line}"
    );
    assert!(line.contains("request=internal"), "{line}");
    assert!(line.contains("attempts=") && !line.contains("attempts=0 "), "{line}");
    assert!(line.contains("waited_ms=") && !line.contains("waited_ms=0 "), "{line}");

    let holder = hold_write_lock(&db, Duration::from_millis(100));
    let err = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id)
                .name("Contended create")
                .build(),
        )
        .unwrap_err();
    holder.join().unwrap();
    assert!(err.downcast_ref::<DuplicateTaskError>().is_some(), "{err:#}");
    assert!(classify(&err).is_none());
}

/// A read that opens a transaction while another connection holds the
/// write lock waits and then succeeds; nothing is returned early or empty.
#[test]
fn read_in_a_transaction_under_external_write_lock_waits_and_succeeds() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let holder = hold_write_lock(&db, Duration::from_millis(250));
    let started = Instant::now();
    let names: Vec<String> = {
        let mut conn = db.connect().unwrap();
        let tx = conn.transaction().unwrap();
        let mut stmt = tx.prepare("SELECT name FROM products").unwrap();
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        drop(stmt);
        tx.commit().unwrap();
        names
    };
    assert!(started.elapsed() >= Duration::from_millis(100), "the read did not wait");
    holder.join().unwrap();
    assert_eq!(names, vec![product.name]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_kind_reaches_the_db_layer_through_the_task_local() {
    assert_eq!(&*current_request_kind(), "internal");
    let kind = REQUEST_KIND
        .scope(Arc::from("ListProducts"), async {
            tokio::task::yield_now().await;
            current_request_kind()
        })
        .await;
    assert_eq!(&*kind, "ListProducts");
    assert_eq!(&*current_request_kind(), "internal");
}

/// A wait past the health threshold shows up in the engine-health snapshot
/// while it lasts, with the holder named, and leaves once it clears; the
/// operation itself still succeeds.
#[test]
fn a_wait_past_the_threshold_is_reported_to_engine_health_until_it_clears() {
    let (_dir, db) = open_db();
    let snapshot = || db.contention_health().snapshot();
    let before = snapshot();
    let holder = hold_write_lock(&db, HEALTH_THRESHOLD + Duration::from_millis(700));
    let writer = thread::spawn({
        let db = db.clone();
        move || db.set_metadata("contended", "value")
    });
    let deadline = Instant::now() + HEALTH_THRESHOLD + Duration::from_millis(500);
    let stuck = loop {
        let stuck = snapshot().stuck;
        if let Some(stuck) = stuck
            .iter()
            .find(|wait| wait.caller.contains("workitems.rs") && wait.request == "internal")
        {
            break stuck.clone();
        }
        assert!(Instant::now() < deadline, "stuck wait never reported: {stuck:?}");
        thread::sleep(Duration::from_millis(50));
    };
    assert!(stuck.attempts > 0);
    assert!(stuck.since.elapsed() >= HEALTH_THRESHOLD);

    writer.join().unwrap().unwrap();
    holder.join().unwrap();
    let after = snapshot();
    assert!(
        after.stuck.iter().all(|wait| !wait.caller.contains("workitems.rs")),
        "{:?}",
        after.stuck
    );
    assert!(after.long_waits > before.long_waits);
    assert!(after.longest_ms >= HEALTH_THRESHOLD.as_millis() as u64);
    assert_eq!(db.get_metadata("contended").unwrap().as_deref(), Some("value"));
}

/// Production connections are files without shared cache and every write
/// transaction begins `IMMEDIATE`, so the two codes SQLite never hands to a
/// busy handler — `SQLITE_LOCKED` and `SQLITE_BUSY_SNAPSHOT` — cannot occur
/// there. A conflict surfaces as plain `SQLITE_BUSY` at `BEGIN`, which the
/// handler absorbs.
#[test]
fn production_connections_conflict_as_plain_busy_at_begin() {
    let (_dir, db) = open_db();
    let holder = hold_write_lock(&db, Duration::from_millis(500));

    let raw = db.connect_new().unwrap();
    raw.busy_timeout(Duration::ZERO).unwrap();
    let err = raw.execute_batch("BEGIN IMMEDIATE").unwrap_err();
    let contention = classify_sqlite(&err).expect("a write conflict is contention");
    assert_eq!(contention.extended_code, ffi::SQLITE_BUSY, "{err}");

    // The default transaction of an engine connection takes the lock at
    // BEGIN, before reading anything it could later fail to upgrade.
    let per_query = db.query_connection().unwrap();
    let mut conn = per_query.connect().unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    let err = conn.transaction().expect_err("the lock is held");
    let contention = classify_sqlite(&err).expect("a write conflict is contention");
    assert_eq!(contention.extended_code, ffi::SQLITE_BUSY, "{err}");
    drop(conn);
    holder.join().unwrap();
}

/// Connections the engine hands out share the database's contention
/// registry, so a stuck wait on any of them reaches the health report.
#[test]
fn every_handle_on_a_database_reports_to_its_health_registry() {
    let (_dir, db) = open_db();
    let registry = db.contention_health();
    assert!(Arc::ptr_eq(registry, db.reader().unwrap().contention_health()));
    assert!(Arc::ptr_eq(
        registry,
        db.query_connection().unwrap().contention_health()
    ));
    assert!(Arc::ptr_eq(registry, db.clone().contention_health()));
}

/// A wait on an independent connection is attributed to the code that opened
/// it and to the request being served, and counted against the database.
#[test]
fn a_wait_on_an_independent_connection_is_attributed_and_counted() {
    let buffer = log_capture::install();
    let (_dir, db) = open_db();
    let waits = db.contention_health().snapshot().waits;
    let from = buffer.lock().len();
    let holder = hold_write_lock(&db, Duration::from_millis(250));
    db.with_independent_connection(|conn| {
        conn.execute_batch("BEGIN IMMEDIATE; COMMIT")?;
        Ok(())
    })
    .unwrap();
    holder.join().unwrap();

    let lines = wait_lines(&buffer, from, "SQLite lock wait cleared");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("contention_tests.rs"), "{}", lines[0]);
    assert!(lines[0].contains("request=internal"), "{}", lines[0]);
    assert_eq!(db.contention_health().snapshot().waits, waits + 1);
}

/// The reported wait ends when the lock is taken, not when the connection
/// guard is released: work done under the lock afterwards is not contention.
#[test]
fn reported_wait_excludes_work_done_after_the_lock_was_taken() {
    let buffer = log_capture::install();
    let (_dir, db) = open_db();
    let from = buffer.lock().len();
    let holder = hold_write_lock(&db, Duration::from_millis(200));
    {
        let mut conn = db.connect().unwrap();
        let tx = conn.transaction().unwrap();
        thread::sleep(Duration::from_millis(900));
        tx.commit().unwrap();
    }
    holder.join().unwrap();
    let lines = wait_lines(&buffer, from, "SQLite lock wait cleared");
    assert_eq!(lines.len(), 1, "{lines:?}");
    let waited_ms: u64 = lines[0]
        .split("waited_ms=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no waited_ms in {}", lines[0]));
    assert!(
        (100..700).contains(&waited_ms),
        "waited_ms={waited_ms} should cover the ~200 ms lock wait, not the 900 ms held after it"
    );
}

/// The CLI constructors give up after a bounded wait instead of hanging on
/// a stuck lock; the engine's own constructors keep waiting until it clears.
#[test]
fn bounded_connections_fail_where_absorbing_ones_wait() {
    let (dir, db) = open_db();
    let path = dir.path().join("state.db");
    let holder = hold_write_lock(&db, Duration::from_millis(1200));

    let bounded = WorkDb::attach_existing(
        path.clone(),
        Arc::default(),
        LockWait::Bounded(Duration::from_millis(150)),
    )
    .unwrap();
    let started = Instant::now();
    let err = bounded
        .connect()
        .unwrap()
        .transaction()
        .expect_err("a bounded wait gives up while the lock is held");
    assert!(classify_sqlite(&err).is_some(), "{err}");
    assert!(started.elapsed() < Duration::from_millis(1000));

    let absorbing = WorkDb::attach_existing(path, Arc::default(), LockWait::Absorb).unwrap();
    absorbing.connect().unwrap().transaction().unwrap().commit().unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "the absorbing wait did not wait"
    );
    holder.join().unwrap();
}

/// A thread that holds the write lock through one engine connection and asks
/// for it through another is waiting for itself. The wait fails loudly once
/// past the health threshold instead of hanging the thread forever.
#[test]
fn a_thread_waiting_for_its_own_write_lock_fails_loudly() {
    let buffer = log_capture::install();
    let (_dir, db) = open_db();
    let from = buffer.lock().len();
    let mut outer = db.connect().unwrap();
    let _outer_tx = outer.transaction().unwrap();

    let other = db.query_connection().unwrap();
    let started = Instant::now();
    let err = other
        .connect()
        .unwrap()
        .transaction()
        .expect_err("a self-deadlock must fail, not hang");
    assert!(classify_sqlite(&err).is_some(), "{err}");
    assert!(started.elapsed() >= HEALTH_THRESHOLD);
    let errors = wait_lines(&buffer, from, "probably waiting for itself");
    assert_eq!(errors.len(), 1, "{errors:?}");
}

/// A stuck wait leaves the health report once it stops retrying, even if the
/// waiting thread keeps its connection guard long afterwards.
#[test]
fn a_stuck_wait_clears_when_it_stops_retrying_not_when_the_guard_drops() {
    let (_dir, db) = open_db();
    let holder = hold_write_lock(&db, HEALTH_THRESHOLD + Duration::from_millis(500));
    let writer = thread::spawn({
        let db = db.clone();
        move || {
            let mut conn = db.connect().unwrap();
            let tx = conn.transaction().unwrap();
            thread::sleep(STALE_AFTER + Duration::from_secs(2));
            tx.commit().unwrap();
        }
    });
    let deadline = Instant::now() + HEALTH_THRESHOLD + Duration::from_millis(500);
    while db.contention_health().snapshot().stuck.is_empty() {
        assert!(Instant::now() < deadline, "stuck wait never reported");
        thread::sleep(Duration::from_millis(50));
    }
    holder.join().unwrap();
    // The lock was taken just after the holder committed; the writer is still
    // inside its transaction but no longer waiting.
    thread::sleep(STALE_AFTER + Duration::from_millis(300));
    assert!(
        db.contention_health().snapshot().stuck.is_empty(),
        "{:?}",
        db.contention_health().snapshot().stuck
    );
    writer.join().unwrap();
}
