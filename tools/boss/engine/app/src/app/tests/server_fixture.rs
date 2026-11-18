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
