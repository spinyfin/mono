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
