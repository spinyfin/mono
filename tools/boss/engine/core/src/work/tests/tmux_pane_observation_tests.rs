//! Durable token-verified `#{pane_dead}` observation on `work_runs`.

use super::*;

use crate::work::{TmuxPaneObservationKind, TmuxPaneObservationRecord};
use rusqlite::Connection;

fn started_tmux_run(db: &WorkDb) -> (String, String) {
    let product = create_test_product(db);
    let chore = create_test_chore(db, product.id.clone(), "Cleanup");
    let execution = create_ready_chore_execution(db, chore.id.clone());
    db.start_execution_run(
        &execution.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution.id, "boss", "boss-worker-1", "tok-1")
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution.id, "tok-1", 4242)
            .unwrap()
    );
    (execution.id, "tok-1".to_owned())
}

fn dead_record() -> TmuxPaneObservationRecord {
    TmuxPaneObservationRecord {
        kind: TmuxPaneObservationKind::Dead,
        pane_dead: Some(true),
        pane_dead_status: Some("0".to_owned()),
        session_name: "boss-worker-1".to_owned(),
    }
}

#[test]
fn migrate_work_runs_tmux_pane_observation_adds_nullable_columns_to_existing_rows() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE work_runs (id TEXT PRIMARY KEY, execution_id TEXT NOT NULL);
         INSERT INTO work_runs (id, execution_id) VALUES ('run_legacy', 'exec_legacy');",
    )
    .unwrap();

    crate::work::migrate_work_runs_tmux_pane_observation(&conn).unwrap();
    crate::work::migrate_work_runs_tmux_pane_observation(&conn).unwrap();

    let columns: Vec<(String, i64)> = conn
        .prepare(
            "SELECT name, \"notnull\" FROM pragma_table_info('work_runs')
             WHERE name IN (
                 'tmux_observed_pane_dead',
                 'tmux_observed_pane_dead_status',
                 'tmux_observed_session_name',
                 'tmux_pane_observation'
             )
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        columns,
        vec![
            ("tmux_observed_pane_dead".to_owned(), 0),
            ("tmux_observed_pane_dead_status".to_owned(), 0),
            ("tmux_observed_session_name".to_owned(), 0),
            ("tmux_pane_observation".to_owned(), 0),
        ],
        "all four observation columns must be present and nullable",
    );

    let (pane_dead, status, session, kind): (Option<i64>, Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT tmux_observed_pane_dead, tmux_observed_pane_dead_status,
                    tmux_observed_session_name, tmux_pane_observation
             FROM work_runs WHERE id = 'run_legacy'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(pane_dead, None);
    assert_eq!(status, None);
    assert_eq!(session, None);
    assert_eq!(kind, None);
}

#[test]
fn pane_observation_survives_identity_clear() {
    let db = WorkDb::open(temp_db_path("tmux-pane-observation")).unwrap();
    let (execution_id, token) = started_tmux_run(&db);
    let record = dead_record();

    let outcome = db
        .record_tmux_pane_observation(&execution_id, &token, &record)
        .unwrap()
        .expect("the spawned run must match");
    assert!(
        outcome.run_id.starts_with("run_"),
        "persist must return the work_runs id, got {}",
        outcome.run_id
    );
    assert!(outcome.written, "first observation on the row must be written");
    assert_eq!(outcome.previous_kind, None, "row had no prior observation");

    assert!(
        db.clear_tmux_identity_for_execution(&execution_id, &token).unwrap(),
        "identity clear is the reap write this record has to outlive",
    );
    assert!(
        db.tmux_identity_for_execution(&execution_id).unwrap().is_none(),
        "precondition: live identity is gone after reap",
    );

    let stored = db
        .tmux_pane_observation_for_execution(&execution_id)
        .unwrap()
        .expect("observation must remain after identity columns are nulled");
    assert_eq!(stored, record);
}

#[test]
fn unreadable_observation_is_not_an_observed_dead_pane() {
    let db = WorkDb::open(temp_db_path("tmux-pane-observation-unreadable")).unwrap();
    let (execution_id, token) = started_tmux_run(&db);
    let unreadable = TmuxPaneObservationRecord {
        kind: TmuxPaneObservationKind::Unreadable,
        pane_dead: None,
        pane_dead_status: None,
        session_name: "boss-worker-1".to_owned(),
    };
    db.record_tmux_pane_observation(&execution_id, &token, &unreadable)
        .unwrap();

    let stored = db
        .tmux_pane_observation_for_execution(&execution_id)
        .unwrap()
        .expect("unreadable must be recorded, not left as never-observed");
    assert_eq!(stored.kind, TmuxPaneObservationKind::Unreadable);
    assert_eq!(stored.pane_dead, None);
    assert_ne!(
        stored.kind,
        TmuxPaneObservationKind::Dead,
        "we could not tell must not look like we observed a clean exit",
    );
}

#[test]
fn session_missing_observation_is_not_an_observed_dead_pane() {
    let db = WorkDb::open(temp_db_path("tmux-pane-observation-absent")).unwrap();
    let (execution_id, token) = started_tmux_run(&db);
    let absent = TmuxPaneObservationRecord {
        kind: TmuxPaneObservationKind::SessionMissing,
        pane_dead: None,
        pane_dead_status: None,
        session_name: "boss-worker-1".to_owned(),
    };
    db.record_tmux_pane_observation(&execution_id, &token, &absent).unwrap();

    let stored = db.tmux_pane_observation_for_execution(&execution_id).unwrap().unwrap();
    assert_eq!(stored.kind, TmuxPaneObservationKind::SessionMissing);
    assert_eq!(stored.pane_dead, None);
}

#[test]
fn stale_spawn_token_matches_no_row_and_writes_nothing() {
    let db = WorkDb::open(temp_db_path("tmux-pane-observation-stale-token")).unwrap();
    let (execution_id, _token) = started_tmux_run(&db);

    // A token that changed between probe and write (e.g. a resume minted a
    // new one) matches no `(execution_id, tmux_spawn_token)` row.
    let outcome = db
        .record_tmux_pane_observation(&execution_id, "stale-tok", &dead_record())
        .unwrap();
    assert!(
        outcome.is_none(),
        "a stale spawn token must match no row, not silently write against the wrong one",
    );
    assert_eq!(
        db.tmux_pane_observation_for_execution(&execution_id).unwrap(),
        None,
        "no observation may be recorded when the token didn't match",
    );
}

#[test]
fn dead_observation_is_not_clobbered_by_a_later_alive_poll() {
    let db = WorkDb::open(temp_db_path("tmux-pane-observation-dead-guard")).unwrap();
    let (execution_id, token) = started_tmux_run(&db);

    let dead_outcome = db
        .record_tmux_pane_observation(&execution_id, &token, &dead_record())
        .unwrap()
        .expect("the spawned run must match");
    assert!(dead_outcome.written, "the first Dead observation must be written");

    let alive = TmuxPaneObservationRecord {
        kind: TmuxPaneObservationKind::Alive,
        pane_dead: Some(false),
        pane_dead_status: None,
        session_name: "boss-worker-1".to_owned(),
    };
    let alive_outcome = db
        .record_tmux_pane_observation(&execution_id, &token, &alive)
        .unwrap()
        .expect("the row still matches");
    assert!(
        !alive_outcome.written,
        "a weaker Alive poll must not overwrite an already-recorded Dead observation",
    );
    assert_eq!(alive_outcome.previous_kind, Some(TmuxPaneObservationKind::Dead));

    let stored = db
        .tmux_pane_observation_for_execution(&execution_id)
        .unwrap()
        .expect("the Dead record must still be present");
    assert_eq!(
        stored,
        dead_record(),
        "the durable row must still read as the original Dead observation",
    );

    // A second Dead observation (e.g. the reap's own probe re-confirming
    // the same exit) is idempotent, not a "weaker kind" refusal.
    let redead_outcome = db
        .record_tmux_pane_observation(&execution_id, &token, &dead_record())
        .unwrap()
        .expect("the row still matches");
    assert!(redead_outcome.written, "re-recording Dead over Dead is not a clobber");
}
