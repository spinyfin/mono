//! Persona leases held by the live-worker registry: terminal workers keep
//! their persona until the slot is released, displaced and re-adopted
//! occupants keep or reclaim theirs, and registration never holds the
//! registry lock while it waits on the database.

use std::sync::Arc;

use boss_protocol::{RequestExecutionInput, WorkExecution, WorkRun};

use super::raw;
use crate::live_worker_state::{LiveSpawnRouting, LiveWorkerStateRegistry, ReadoptionEvidence};
use crate::test_support::{create_test_chore_manual, create_test_product, open_db};
use crate::work::WorkDb;

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
fn displaced_registry_occupant_releases_its_persona_lease() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let states = LiveWorkerStateRegistry::with_work_db(db.clone());
    let (first, _) = start(&db, "local");
    let (second, _) = start(&db, "local");
    states.register_spawn(8, &first.id, "model", 123, None);
    // Re-registering the slot for a different run displaces `first` without a
    // `release_slot`; its lease must be reclaimed, the new occupant's kept.
    states.register_spawn(8, &second.id, "model", 124, None);
    let (_, third) = start(&db, "local");
    assert_eq!(third.persona.as_deref(), Some("Riker"), "displaced persona reused");
    assert_eq!(db.persona_display_name(&first.id).unwrap().as_deref(), Some("Riker"));
    assert_eq!(states.get(8).unwrap().name, "Data");
}

/// Dispatch holds the DB connection and then asks the registry whether a run is
/// live. Spawn registration must therefore never hold the registry lock while
/// waiting for the DB connection.
#[test]
fn spawn_registration_does_not_hold_registry_lock_while_waiting_for_db() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let states = Arc::new(LiveWorkerStateRegistry::with_work_db(db.clone()));
    let (execution, _) = start(&db, "local");
    // Hold the shared connection the way a dispatch transaction does. No
    // public `WorkDb` operation holds it across a caller-supplied wait, so
    // this one test still takes the crate-private guard.
    let conn = db.connect().unwrap();
    let (boundary_tx, boundary_rx) = std::sync::mpsc::channel();
    *states.test_hooks.persona_boundary.lock().unwrap() = Some(Box::new(move || {
        let _ = boundary_tx.send(());
    }));
    let spawner = {
        let states = states.clone();
        let run_id = execution.id.clone();
        std::thread::spawn(move || states.register_spawn(8, &run_id, "model", 123, None))
    };
    let boundary = boundary_rx.recv_timeout(std::time::Duration::from_secs(5));
    let (tx, rx) = std::sync::mpsc::channel();
    let probe = {
        let states = states.clone();
        std::thread::spawn(move || {
            let _ = tx.send((states.is_run_live("some-run"), states.snapshot().len()));
        })
    };
    let result = rx.recv_timeout(std::time::Duration::from_secs(5));
    drop(conn);
    probe.join().unwrap();
    spawner.join().unwrap();
    assert!(boundary.is_ok(), "spawner never reached the persona DB boundary");
    assert!(
        result.is_ok(),
        "registry lock was held while waiting on the DB connection"
    );
    assert_eq!(states.get(8).unwrap().name, "Riker");
}

#[test]
fn same_run_readoption_waits_for_removed_entry_persona_release() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let states = Arc::new(LiveWorkerStateRegistry::with_work_db(db.clone()));
    let (execution, run) = start(&db, "local");
    states.register_spawn(8, &execution.id, "model", 123, None);
    let (removed_tx, removed_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    *states.test_hooks.persona_boundary.lock().unwrap() = Some(Box::new(move || {
        let _ = removed_tx.send(());
        let _ = resume_rx.recv_timeout(std::time::Duration::from_secs(5));
    }));
    let releaser = {
        let states = states.clone();
        let id = execution.id.clone();
        std::thread::spawn(move || states.release_slot_for_run(&id))
    };
    let removed = removed_rx.recv_timeout(std::time::Duration::from_secs(5));
    let absent = states.get(8).is_none();
    let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
    *states.test_hooks.lifecycle_waiter.lock().unwrap() = Some(waiting_tx);
    let readopter = {
        let states = states.clone();
        let id = execution.id.clone();
        std::thread::spawn(move || {
            states.register_readoption(
                8,
                id,
                "model",
                123,
                None,
                false,
                LiveSpawnRouting::none(),
                ReadoptionEvidence::LiveShellPid,
            );
        })
    };
    let waiting = waiting_rx.recv_timeout(std::time::Duration::from_secs(5));
    let _ = resume_tx.send(());
    releaser.join().unwrap();
    readopter.join().unwrap();
    assert!(removed.is_ok() && waiting.is_ok() && absent);
    assert_eq!(states.get(8).unwrap().name, "Riker");
    let active: bool = raw(&db)
        .query_row(
            "SELECT persona_lease_active FROM work_runs WHERE id = ?1",
            [&run.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(active, "surviving registry entry must retain its persona lease");
    let (_, next) = start(&db, "local");
    assert_eq!(next.persona.as_deref(), Some("Data"));
}
