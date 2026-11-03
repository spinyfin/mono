use super::*;

const DAY: i64 = 86400;

fn no_live_pid(_: Option<i64>) -> bool {
    false
}

fn snapshot_rows(db: &WorkDb) -> Vec<(i64, String)> {
    let conn = db.connect().unwrap();
    let mut statement = conn
        .prepare("SELECT day, report_json FROM task_stuck_daily_snapshots ORDER BY day")
        .unwrap();
    statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn stores_one_report_per_utc_day_and_replays_it() {
    let db = WorkDb::open(temp_db_path("stuck-snapshot-replay")).unwrap();
    let product = create_test_product_named(&db, "Snapshots");
    create_test_chore_manual(&db, &product.id, "First");
    let day_start = 20_000 * DAY;

    let first = db.stuck_daily_snapshot(day_start + 10, &no_live_pid).unwrap();
    create_test_chore_manual(&db, &product.id, "Added later the same day");
    let replay = db.stuck_daily_snapshot(day_start + DAY - 1, &no_live_pid).unwrap();

    assert_eq!(replay.rows.len(), first.rows.len());
    assert_eq!(replay.generated_at_epoch_s, first.generated_at_epoch_s);
    assert_eq!(snapshot_rows(&db).len(), 1);
}

#[test]
fn utc_day_rollover_stores_a_new_report() {
    let db = WorkDb::open(temp_db_path("stuck-snapshot-rollover")).unwrap();
    let product = create_test_product_named(&db, "Rollover");
    create_test_chore_manual(&db, &product.id, "Before midnight");
    let day = 20_000;

    let before = db.stuck_daily_snapshot(day * DAY + DAY - 1, &no_live_pid).unwrap();
    create_test_chore_manual(&db, &product.id, "After midnight");
    let after = db.stuck_daily_snapshot((day + 1) * DAY, &no_live_pid).unwrap();

    assert_eq!(after.rows.len(), before.rows.len() + 1);
    let days: Vec<i64> = snapshot_rows(&db).into_iter().map(|(d, _)| d).collect();
    assert_eq!(days, vec![day, day + 1]);
}

#[test]
fn concurrent_samplers_converge_on_one_stored_report() {
    // Concurrent writers need an on-disk database (WAL + busy_timeout).
    let (_dir, path) = disk_db_path("stuck-snapshot-concurrent");
    let db = std::sync::Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product_named(&db, "Concurrent");
    create_test_chore_manual(&db, &product.id, "Item");
    let now = 20_000 * DAY + 123;

    let handles: Vec<_> = (0..8)
        .map(|offset| {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || db.stuck_daily_snapshot(now + offset, &no_live_pid).unwrap())
        })
        .collect();
    let reports: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let rows = snapshot_rows(&db);
    assert_eq!(rows.len(), 1);
    let stored: boss_protocol::StuckReport = serde_json::from_str(&rows[0].1).unwrap();
    for report in reports {
        assert_eq!(report.generated_at_epoch_s, stored.generated_at_epoch_s);
    }
}

#[test]
fn report_build_does_not_take_the_shared_writer_connection() {
    let (_dir, path) = disk_db_path("stuck-snapshot-reader");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_named(&db, "Reader");
    create_test_chore_manual(&db, &product.id, "Item");
    let now = 20_000 * DAY + 5;
    // Holding the shared connection here would deadlock a read phase that
    // used it (the mutex is not reentrant).
    let guard = db.connect().unwrap();
    let read = db.stuck_snapshot_read_phase(now.div_euclid(DAY), now).unwrap();
    drop(guard);
    assert!(matches!(read, crate::stuck_snapshot::SnapshotRead::Built(..)));
    // A write through the original handle still completes afterwards.
    create_test_chore_manual(&db, &product.id, "Written after");
}

#[test]
fn undecodable_stored_row_is_replaced() {
    let db = WorkDb::open(temp_db_path("stuck-snapshot-corrupt")).unwrap();
    let now = 20_000 * DAY + 5;
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO task_stuck_daily_snapshots(day, report_json) VALUES (?1, '{\"bad\":1}')",
            [now.div_euclid(DAY)],
        )
        .unwrap();
    db.stuck_daily_snapshot(now, &no_live_pid).unwrap();
    let again = db.stuck_daily_snapshot(now + 1, &no_live_pid).unwrap();
    assert_eq!(again.generated_at_epoch_s, now);
}
