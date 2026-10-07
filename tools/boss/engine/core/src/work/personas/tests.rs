use super::*;
use crate::live_worker_state::{LiveSpawnRouting, LiveWorkerStateRegistry, ReadoptionEvidence};
use crate::test_support::{create_test_chore_manual, create_test_product, open_db};

fn start(db: &WorkDb, host: &str) -> (WorkExecution, WorkRun) {
    let product = create_test_product(db);
    let chore = create_test_chore_manual(db, product.id, "persona worker");
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id).build())
        .unwrap();
    db.start_execution_run_on_host_with_tmux_hosting(
        &execution.id,
        "worker-8",
        "mono",
        "lease",
        "workspace",
        "/tmp/workspace",
        host,
        host == "local",
    )
    .unwrap()
}

fn handle(execution: &WorkExecution, run: &WorkRun) -> TmuxRunHandle {
    TmuxRunHandle::builder()
        .run_id(&run.id)
        .execution_id(&execution.id)
        .agent_id("worker-8")
        .tmux_server_label("boss")
        .tmux_session_name("session")
        .tmux_spawn_token(&run.id)
        .tmux_spawn_state("created")
        .build()
}

#[test]
fn spawn_transaction_allocates_one_roster_across_local_and_remote_runs() {
    let (_dir, db) = open_db();
    let (local, local_run) = start(&db, "local");
    let (remote, remote_run) = start(&db, "remote-host");
    assert_eq!(local_run.persona.as_deref(), Some("Riker"));
    assert_eq!(remote_run.persona.as_deref(), Some("Data"));
    assert_eq!(db.persona_display_name(&local.id).unwrap().as_deref(), Some("Riker"));
    assert_eq!(
        db.persona_display_name(&remote.id).unwrap().as_deref(),
        Some("Data (Remote)")
    );
    assert_eq!(db.list_runs(&local.id).unwrap()[0].persona, local_run.persona);
}

#[test]
fn overflow_is_unique_reusable_and_counted_only_on_allocation() {
    let (_dir, db) = open_db();
    let mut executions = Vec::new();
    let mut names = HashSet::new();
    // Full local capacity plus the entire remote virtual-slot range.
    for index in 0..96 {
        let (execution, run) = start(&db, if index < 40 { "local" } else { "remote-host" });
        assert!(names.insert(run.persona.unwrap()));
        executions.push(execution);
    }
    assert!(names.contains("Ensign 56"));
    assert_eq!(db.persona_metrics.counter_value("persona_roster_exhausted"), Some(56));
    assert_eq!(
        db.lease_persona_for_execution(&executions[40].id).unwrap(),
        "Ensign 1 (Remote)"
    );
    assert_eq!(db.persona_metrics.counter_value("persona_roster_exhausted"), Some(56));
    db.release_persona(&executions[40].id).unwrap();
    let (_, replacement) = start(&db, "local");
    assert_eq!(replacement.persona.as_deref(), Some("Ensign 1"));
    assert_eq!(db.persona_metrics.counter_value("persona_roster_exhausted"), Some(57));
}

#[test]
fn terminal_state_holds_persona_until_live_slot_release() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let states = LiveWorkerStateRegistry::with_work_db(db.clone());
    let (first, _) = start(&db, "local");
    states.register_spawn(8, &first.id, "model", 123, None);
    states.apply_event(
        8,
        &boss_protocol::WorkerEvent::SessionEnd {
            session_id: "session".into(),
            reason: "ended".into(),
        },
    );
    db.cancel_running_execution(&first.id).unwrap();
    assert!(states.get(8).unwrap().activity.is_terminal());
    let (_, second) = start(&db, "local");
    assert_eq!(second.persona.as_deref(), Some("Data"));
    states.release_slot(8);
    let (_, third) = start(&db, "local");
    assert_eq!(third.persona.as_deref(), Some("Riker"));
    assert_eq!(db.persona_display_name(&first.id).unwrap().as_deref(), Some("Riker"));
}

#[test]
fn restart_restores_persisted_leases_before_ordered_legacy_rows() {
    let (dir, db) = open_db();
    let (old, old_run) = start(&db, "local");
    let (persisted, persisted_run) = start(&db, "remote-host");
    let (newer, newer_run) = start(&db, "local");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_runs SET persona = NULL, persona_lease_active = 0 WHERE id IN (?1, ?2)",
            params![old_run.id, newer_run.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_runs SET persona = 'Riker' WHERE id = ?1",
            [&persisted_run.id],
        )
        .unwrap();
    }
    let path = db.path.clone();
    drop(db);
    let db = WorkDb::open(path).unwrap();
    db.restore_tmux_personas(&[handle(&old, &old_run), handle(&newer, &newer_run)])
        .unwrap();
    assert_eq!(db.lease_persona_for_execution(&persisted.id).unwrap(), "Riker (Remote)");
    assert_eq!(db.lease_persona_for_execution(&old.id).unwrap(), "Data");
    assert_eq!(db.lease_persona_for_execution(&newer.id).unwrap(), "Worf");
    assert!(dir.path().exists());
}

#[test]
fn same_run_readoption_preserves_name_and_state_across_slot_changes() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let (execution, _) = start(&db, "local");
    let states = LiveWorkerStateRegistry::with_work_db(db.clone());
    states.register_spawn(8, &execution.id, "original-model", 123, None);
    states.set_held(&execution.id, true);
    states.register_readoption(
        8,
        &execution.id,
        "new-model",
        456,
        None,
        false,
        LiveSpawnRouting::none(),
        ReadoptionEvidence::LiveShellPid,
    );
    let state = states.get(8).unwrap();
    assert_eq!(state.name, "Riker");
    assert_eq!(state.model, "original-model");
    assert!(state.held);
    let restored = LiveWorkerStateRegistry::with_work_db(db);
    restored.register_readoption(
        27,
        &execution.id,
        "model",
        456,
        None,
        false,
        LiveSpawnRouting::none(),
        ReadoptionEvidence::LiveShellPid,
    );
    assert_eq!(restored.get(27).unwrap().name, "Riker");
}

#[test]
fn persisted_names_are_restored_before_unnamed_rows_and_released_conflicts_are_reassigned() {
    let (_dir, db) = open_db();
    let (old, old_run) = start(&db, "local");
    let (saved, saved_run) = start(&db, "local");
    db.release_persona(&old.id).unwrap();
    db.release_persona(&saved.id).unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE work_runs SET persona = NULL WHERE id = ?1", [&old_run.id])
        .unwrap();
    db.restore_tmux_personas(&[handle(&old, &old_run), handle(&saved, &saved_run)])
        .unwrap();
    assert_eq!(db.lease_persona_for_execution(&saved.id).unwrap(), "Data");
    assert_eq!(db.lease_persona_for_execution(&old.id).unwrap(), "Riker");
    db.release_persona(&old.id).unwrap();
    let (_, replacement) = start(&db, "local");
    assert_eq!(replacement.persona.as_deref(), Some("Riker"));
    assert_eq!(db.lease_persona_for_execution(&old.id).unwrap(), "Worf");
}

#[test]
fn allocation_rolls_back_and_database_rejects_duplicate_live_leases() {
    let (_dir, db) = open_db();
    let (first, first_run) = start(&db, "local");
    let (_, second_run) = start(&db, "local");
    db.release_persona(&first.id).unwrap();
    let mut conn = db.connect().unwrap();
    {
        let tx = conn.transaction().unwrap();
        allocate(&tx, &first_run.id).unwrap();
        // Intentionally roll back the whole spawn/restore transaction.
    }
    let active: bool = conn
        .query_row(
            "SELECT persona_lease_active FROM work_runs WHERE id = ?1",
            [&first_run.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!active);
    assert!(
        conn.execute(
            "UPDATE work_runs SET persona = 'Data', persona_lease_active = 1 WHERE id = ?1",
            [&first_run.id]
        )
        .is_err()
    );
    assert_eq!(second_run.persona.as_deref(), Some("Data"));
}

#[test]
fn concurrent_spawns_share_the_same_lease_namespace() {
    let (_dir, db) = open_db();
    let threads: Vec<_> = (0..16)
        .map(|index| {
            let db = db.clone();
            std::thread::spawn(move || {
                start(&db, if index % 2 == 0 { "local" } else { "remote-host" })
                    .1
                    .persona
                    .unwrap()
            })
        })
        .collect();
    let names: HashSet<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert_eq!(names.len(), 16);
}

#[test]
fn bookkeeping_sibling_does_not_shadow_worker_persona_and_untracked_cleanup_releases_it() {
    let (_dir, db) = open_db();
    let (execution, run) = start(&db, "local");
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at)
         VALUES ('bookkeeping', ?1, 'system', 'failed', '9999-01-01')",
            [&execution.id],
        )
        .unwrap();
    assert_eq!(db.lease_persona_for_execution(&execution.id).unwrap(), "Riker");
    db.clear_execution_workspace(&execution.id).unwrap();
    let (_, replacement) = start(&db, "local");
    assert_eq!(replacement.persona, run.persona);
    assert_eq!(
        db.persona_display_name(&execution.id).unwrap().as_deref(),
        Some("Riker")
    );
}

#[test]
fn held_persona_is_readable_when_adoption_cannot_write() {
    let (_dir, db) = open_db();
    let (execution, _) = start(&db, "local");
    db.connect().unwrap().execute_batch("PRAGMA query_only = ON").unwrap();
    assert_eq!(db.lease_persona_for_execution(&execution.id).unwrap(), "Riker");
}
