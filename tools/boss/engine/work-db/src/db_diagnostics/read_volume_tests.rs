//! Read accounting against a file database larger than the page cache.
use super::*;
use crate::WorkDb;

/// A table of `rows` 2 KiB rows: two rows per page, so a walk of it reads
/// about `rows / 2` pages on a connection that has not cached them.
fn seeded(rows: usize) -> (tempfile::TempDir, WorkDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("reads.db")).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE read_probe(id INTEGER PRIMARY KEY, tag TEXT NOT NULL, filler TEXT NOT NULL);
             CREATE INDEX read_probe_tag ON read_probe(tag);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {rows})
             INSERT INTO read_probe SELECT i, 'tag' || i, printf('%.1900c', 'x') FROM n;"
        ))
        .unwrap();
    }
    (dir, db)
}

fn reads_of(db: &WorkDb, work: impl FnOnce(&rusqlite::Connection)) -> HoldReads {
    // A reader is a new connection with an empty cache, as a bulk RPC gets.
    let reader = db.reader().unwrap();
    let conn = reader.connect().unwrap();
    let id = unsafe { conn.handle() } as usize;
    work(&conn);
    let observation = crate::db_diagnostics::ACTIVE.with_borrow(|active| active.get(&id).cloned().unwrap());
    observation.finish_reads(&conn)
}

#[test]
fn a_table_walk_is_charged_to_its_statement_and_a_seek_is_not() {
    let (_dir, db) = seeded(4000);
    let scan = reads_of(&db, |conn| {
        let n: i64 = conn
            .query_row("SELECT count(*) FROM read_probe WHERE length(filler) > 0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 4000);
    });
    assert!(scan.pages >= 1500, "a 4000-row walk reads the table: {scan:?}");
    assert_eq!(scan.statements, 1);
    assert!(scan.full_scan_steps >= 3999, "{scan:?}");
    let top = scan.top.clone().expect("the walk is tracked by text");
    assert_eq!(top.runs, 1);
    assert!(top.sql.starts_with("SELECT count(*) FROM read_probe"), "{}", top.sql);
    assert!(top.pages >= 1500 && top.pages <= scan.pages, "{top:?}");

    let seek = reads_of(&db, |conn| {
        let tag: String = conn
            .query_row("SELECT tag FROM read_probe WHERE id = 2000", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tag, "tag2000");
    });
    // A new connection also reads the schema before its first statement.
    assert!(
        seek.pages * 10 < scan.pages,
        "a primary-key seek reads a fraction of the walk: {seek:?} vs {scan:?}"
    );
    assert_eq!(seek.full_scan_steps, 0);
    assert_eq!(seek.top, None, "below the tracking floor");
}

#[test]
fn the_same_text_run_in_a_loop_is_one_entry_with_many_runs() {
    let (_dir, db) = seeded(4000);
    let looped = reads_of(&db, |conn| {
        for _ in 0..3 {
            // Uncached prepare: a new statement object each time.
            let n: i64 = conn
                .prepare("SELECT count(*) FROM read_probe WHERE length(filler) > 0")
                .unwrap()
                .query_row([], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 4000);
        }
    });
    let top = looped.top.clone().expect("tracked");
    assert_eq!(top.runs, 3, "{top:?}");
    assert_eq!(looped.statements, 3);
    assert!(
        looped.pages >= 3 * 1500,
        "the cache is smaller than the table: {looped:?}"
    );
}

#[test]
fn a_statement_counter_keeps_read_accounting_running() {
    let (_dir, db) = seeded(4000);
    let reads = reads_of(&db, |conn| {
        let (n, statements) = crate::db_diagnostics::count_statements(conn, || {
            conn.query_row("SELECT count(*) FROM read_probe WHERE length(filler) > 0", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        });
        assert_eq!((n, statements), (4000, 1));
        // After the counter is dropped the read callback is back in place.
        conn.query_row("SELECT count(*) FROM read_probe WHERE length(tag) > 0", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    });
    assert_eq!(reads.statements, 2, "{reads:?}");
    assert!(reads.top.is_some_and(|top| top.pages >= 1500));
}

#[test]
fn heavy_holds_are_named_in_the_summary_with_their_caller() {
    // The window is process-wide; other tests add callers but never remove
    // this one before it is drained here.
    let (_dir, db) = seeded(4000);
    let reader = db.reader().unwrap();
    let caller = {
        let conn = reader.connect().unwrap();
        let caller = std::panic::Location::caller();
        let _: i64 = conn
            .query_row("SELECT count(*) FROM read_probe WHERE length(filler) > 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        caller
    };
    let (_, _, callers) = take_summary();
    let mine = callers
        .iter()
        .find(|c| {
            c.caller.starts_with(caller.file()) && c.top_statement.as_ref().is_some_and(|s| s.sql.contains("> 1"))
        })
        .unwrap_or_else(|| panic!("caller missing from {callers:?}"));
    assert_eq!(mine.holds, 1);
    assert!(mine.pages >= 1500, "{mine:?}");
    assert_eq!(mine.heaviest_hold_pages, mine.pages);
}

#[test]
fn only_a_query_over_the_threshold_is_warned_about() {
    let threshold = heavy_hold_pages();
    assert!(warrants_warning(threshold, Scope::Query));
    assert!(!warrants_warning(threshold - 1, Scope::Query));
    // A backup snapshot copies every page by design.
    assert!(!warrants_warning(u64::MAX, Scope::WholeFile));
}

#[test]
fn previews_collapse_whitespace_and_truncate() {
    assert_eq!(preview("SELECT 1\n   FROM   t"), "SELECT 1 FROM t");
    let long = preview(&"word ".repeat(200));
    assert!(long.ends_with("..."));
    assert_eq!(long.chars().count(), SQL_PREVIEW_CHARS + 3);
}
