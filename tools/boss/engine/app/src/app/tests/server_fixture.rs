use super::*;

#[test]
fn prepared_databases_keep_server_state_isolated_and_persistent() {
    let (first, first_dir) = test_server_state();
    let (second, second_dir) = test_server_state_with_fakes();
    assert_ne!(first_dir.path(), second_dir.path());
    for (state, dir) in [(&first, &first_dir), (&second, &second_dir)] {
        assert_eq!(state.work_db.path(), dir.path().join("state.db"));
        assert!(Arc::ptr_eq(state.work_db.event_bus(), &state.event_bus));
        let conn = state.work_db.connect().unwrap();
        let journal: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
        assert_eq!(journal, "wal");
        let foreign_keys: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).unwrap();
        assert!(foreign_keys);
    }
    first.work_db.set_metadata("fixture-only", "first").unwrap();
    assert_eq!(second.work_db.get_metadata("fixture-only").unwrap(), None);
    drop(first);
    let reopened = WorkDb::open_existing(first_dir.path().join("state.db")).unwrap();
    assert_eq!(reopened.get_metadata("fixture-only").unwrap().as_deref(), Some("first"));
}

/// Nothing a server state builds may keep its coordinator, or its database
/// connection and page cache, alive after it is dropped. Every test here
/// builds at least one server state; a strong scheduler-kick handle would
/// cycle the coordinator and the publisher it owns.
#[test]
fn a_dropped_server_state_frees_its_coordinator_and_database() {
    for (state, _dir) in [test_server_state(), test_server_state_with_fakes()] {
        let work_db = Arc::downgrade(&state.work_db);
        let coordinator = Arc::downgrade(&state.execution_coordinator);
        let completion_handler = Arc::downgrade(&state.completion_handler);
        drop(state);
        assert_eq!(coordinator.strong_count(), 0, "execution coordinator");
        assert_eq!(completion_handler.strong_count(), 0, "completion handler");
        assert_eq!(work_db.strong_count(), 0, "work database");
    }
}
