use super::*;
use crate::test_support::log_capture;
use crate::work::WorkDb;

#[tokio::test]
async fn attributed_hold_excludes_non_sql_work_from_query_time() {
    let logs = log_capture::install();
    let directory = tempfile::tempdir().unwrap();
    let db = WorkDb::open(directory.path().join("timing.db")).unwrap();
    let from = logs.lock().len();
    Attribution {
        verb: "diagnostic_slow_section".into(),
        request_id: "request-timing".into(),
        session_id: "session-timing".into(),
        background: String::new(),
    }
    .scope(async {
        let conn = db.connect().unwrap();
        conn.query_row("SELECT 1", [], |_| Ok(())).unwrap();
        let id = unsafe { conn.handle() } as usize;
        let before = ACTIVE.with_borrow(|active| active[&id].query_time());
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(before, ACTIVE.with_borrow(|active| active[&id].query_time()));
    })
    .await;
    let text = String::from_utf8_lossy(&logs.lock()[from..]).into_owned();
    let line = text
        .lines()
        .find(|line| line.contains("diagnostic_slow_section") && line.contains("work DB connection lock timing"))
        .unwrap();
    assert!(
        line.contains("request-timing") && line.contains("session-timing"),
        "{line}"
    );
    let number = |field: &str| -> f64 {
        line.split_once(field)
            .unwrap()
            .1
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let query_ms = number("query_ms=");
    let held_ms = number("held_ms=");
    assert!(query_ms > 0.0, "{line}");
    assert!(
        held_ms - query_ms >= 100.0,
        "non-SQL work must not count as query time: {line}"
    );
}

#[tokio::test]
async fn busy_names_actual_writer_and_counts_rate_limited_failures() {
    let logs = log_capture::install();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("busy.db");
    let writer = WorkDb::open(path.clone()).unwrap();
    let reader = WorkDb::open_existing(path).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let release = barrier.clone();
    let from = logs.lock().len();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(
            Attribution {
                background: "diagnostic_writer".into(),
                ..Attribution::default()
            }
            .scope(async {
                // Observe the connection on this thread; `connect_new` itself
                // returns a raw, movable connection that diagnostics ignore.
                let mut conn = OwnedConnection::new(writer.connect_new().unwrap(), writer.persona_metrics.clone());
                let tx = conn.transaction().unwrap();
                assert!(holder_snapshot().iter().any(|line| line.contains("diagnostic_writer")));
                release.wait();
                release.wait();
                tx.rollback().unwrap();
                assert!(holder_snapshot().iter().all(|line| !line.contains("diagnostic_writer")));
            }),
        );
    });
    barrier.wait();
    std::thread::sleep(Duration::from_millis(20));
    Attribution {
        verb: "diagnostic_busy".into(),
        request_id: "busy-request".into(),
        session_id: "busy-session".into(),
        background: String::new(),
    }
    .scope(async {
        let conn = reader.connect().unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        let id = unsafe { conn.handle() } as usize;
        let caller = ACTIVE.with_borrow(|active| active[&id].caller);
        let before = BUSY.lock().unwrap().get(caller).map_or(0, |count| count.total);
        for _ in 0..3 {
            let error = conn.execute_batch("BEGIN IMMEDIATE").unwrap_err();
            assert_eq!(error.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy));
            // A later successful statement must not hide the failed statement.
            conn.query_row("SELECT 1", [], |_| Ok(())).unwrap();
        }
        let counts = BUSY.lock().unwrap();
        assert_eq!(counts[caller].total - before, 3);
        assert_eq!(counts[caller].suppressed, 2);
        assert_eq!(reader.persona_metrics.counter_value("work_database_busy"), Some(3));
    })
    .await;
    barrier.wait();
    thread.join().unwrap();
    let text = String::from_utf8_lossy(&logs.lock()[from..]).into_owned();
    let lines: Vec<_> = text
        .lines()
        .filter(|line| line.contains("diagnostic_busy") && line.contains("SQLITE_BUSY holder diagnostic"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(
        lines[0].contains("diagnostic_writer") && lines[0].contains("held_ms="),
        "{}",
        lines[0]
    );
    let holder_ms: u64 = lines[0]
        .split_once("held_ms=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap();
    assert!(holder_ms >= 20, "{}", lines[0]);
    assert!(
        lines[0].contains("busy-request") && lines[0].contains("busy-session"),
        "{}",
        lines[0]
    );
    assert!(
        HOLDERS
            .lock()
            .unwrap()
            .values()
            .all(|holder| holder.attribution.background != "diagnostic_writer")
    );
}

#[tokio::test]
async fn spawned_work_inherits_the_request_context() {
    Attribution {
        verb: "inherited_request".into(),
        ..Attribution::default()
    }
    .scope(async {
        spawn(async {
            assert_eq!(CONTEXT.with(|context| context.verb.clone()), "inherited_request");
        })
        .await
        .unwrap();
    })
    .await;
    spawn(async {
        assert!(CONTEXT.with(|context| context.background.contains("db_diagnostics/tests.rs")));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn blocking_work_inherits_the_request_context() {
    Attribution {
        verb: "blocking_request".into(),
        ..Attribution::default()
    }
    .scope(async {
        spawn_blocking(|| assert_eq!(CONTEXT.with(|context| context.verb.clone()), "blocking_request"))
            .await
            .unwrap();
    })
    .await;
}
