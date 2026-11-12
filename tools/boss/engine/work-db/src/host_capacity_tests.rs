use super::*;
use boss_host_execution::capacity::{DISK_FLOOR_BYTES, WORKER_DISK_BYTES, WORKER_MEMORY_BYTES};

fn fixture() -> (WorkDb, i64) {
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    db.add_host("host", "", 4, &[]).unwrap();
    db.set_host_transport("host", HostTransport::SpokeReady).unwrap();
    db.open_spoke_journal("host", "journal", 0, 1).unwrap();
    let now = now_ms();
    db.open_host_capacity("host", 1, 0, now).unwrap();
    db.host_capacity_inventory("host", 1, 1, 0).unwrap();
    // The projector has applied the inventory barrier.
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO spoke_projection(host_id, journal_id, applied) VALUES ('host', 'journal', 1)",
            [],
        )
        .unwrap();
    (db, now)
}

fn report() -> HostCapacityReport {
    HostCapacityReport::builder()
        .generation(2)
        .sequence(2)
        .boot_id("boot")
        .awake_ns(20)
        .total(4)
        .occupied(0)
        .resources(Resources {
            cpu_millis: 4000,
            memory_bytes: 4 * WORKER_MEMORY_BYTES,
            disk_bytes: DISK_FLOOR_BYTES + 4 * WORKER_DISK_BYTES,
        })
        .applied_command_through(0)
        .preflights_ok(true)
        .build()
}

fn ready(db: &WorkDb, now: i64) {
    db.host_capacity_pulse("host", 1, "boot", 10, now).unwrap();
    db.record_host_capacity("host", 1, &report(), now).unwrap();
}

#[test]
fn capacity_requires_current_challenge_replay_and_projection_barriers() {
    let (db, now) = fixture();
    db.record_host_capacity("host", 1, &report(), now).unwrap();
    assert_eq!(available(&db.connect().unwrap(), "host", now).unwrap(), 0);
    ready(&db, now);
    assert_eq!(available(&db.connect().unwrap(), "host", now).unwrap(), 4);
    db.connect()
        .unwrap()
        .execute("UPDATE spoke_projection SET applied = 0", [])
        .unwrap();
    assert_eq!(available(&db.connect().unwrap(), "host", now).unwrap(), 0);
    db.connect()
        .unwrap()
        .execute("UPDATE spoke_projection SET applied = 1", [])
        .unwrap();
    db.host_capacity_disconnected("host", 1, now).unwrap();
    assert_eq!(available(&db.connect().unwrap(), "host", now).unwrap(), 0);
}

#[test]
fn simultaneous_admissions_cannot_spend_the_same_budget() {
    let (db, now) = fixture();
    ready(&db, now);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    let winners = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let db = db.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    db.admit_spoke_assignment(&format!("execution-{i}"), "host")
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap().ok())
            .collect::<Vec<_>>()
    });
    assert_eq!(winners.len(), 4);
    assert_eq!(db.available_host_budget("host").unwrap(), 0);
    let winner = &winners[0];
    assert_eq!(
        db.admit_spoke_assignment(&winner.execution_id, "host").unwrap(),
        *winner
    );
    assert!(
        db.resolve_spoke_assignment(&winner.execution_id, winner.assignment_generation, "never_started")
            .unwrap()
    );
    assert!(
        !db.resolve_spoke_assignment(&winner.execution_id, winner.assignment_generation, "never_started")
            .unwrap()
    );
    assert_eq!(db.available_host_budget("host").unwrap(), 1);
}

#[test]
fn prefix_accounts_for_running_work_once_and_pending_assignments_separately() {
    let (db, now) = fixture();
    ready(&db, now);
    let first = db.admit_spoke_assignment("first", "host").unwrap();
    let key = SpokeRunKey {
        execution_id: first.execution_id,
        assignment_generation: first.assignment_generation,
        run_id: first.run_id.clone(),
        spawn_token: first.spawn_token,
    };
    db.resume_host_command_journal("host", None).unwrap();
    let command = db
        .append_host_command(
            "host",
            &crate::spoke_hub::host_command_spawn_id(&first.run_id),
            Some(&key),
            RunAdmission::Start,
            b"spawn",
            &|_| b"spawn".to_vec(),
        )
        .unwrap();
    db.admit_spoke_assignment("second", "host").unwrap();
    assert_eq!(db.available_host_budget("host").unwrap(), 2);
    let mut updated = report();
    updated.generation = 3;
    updated.sequence = 3;
    updated.applied_command_through = command.sequence;
    updated.occupied = 1;
    updated.resources.cpu_millis = 3000;
    db.record_host_capacity("host", 1, &updated, now).unwrap();
    assert_eq!(
        db.available_host_budget("host").unwrap(),
        2,
        "count the first run only in the report"
    );
    db.record_host_capacity("host", 1, &report(), now).unwrap();
    assert_eq!(
        db.available_host_budget("host").unwrap(),
        2,
        "older capacity cannot restore spent resources"
    );
}

#[test]
fn replay_staleness_boot_and_connection_fences_fail_closed() {
    let (db, now) = fixture();
    ready(&db, now);
    let later = now + HOST_FRESHNESS_MS;
    db.host_capacity_pulse("host", 1, "boot", 30, later).unwrap();
    db.record_host_capacity("host", 1, &report(), later).unwrap();
    assert_eq!(
        available(&db.connect().unwrap(), "host", later).unwrap(),
        0,
        "duplicate reports do not renew freshness"
    );
    let mut old_sample = report();
    old_sample.generation = 3;
    db.record_host_capacity("host", 1, &old_sample, later).unwrap();
    assert_eq!(
        available(&db.connect().unwrap(), "host", later).unwrap(),
        0,
        "buffered sample predates the challenge"
    );
    db.open_host_capacity("host", 2, 10, later).unwrap();
    ready(&db, later);
    assert_eq!(
        available(&db.connect().unwrap(), "host", later).unwrap(),
        0,
        "previous stream is fenced"
    );
    db.host_capacity_pulse("host", 2, "new-boot", 1, later).unwrap();
    old_sample.sequence = 11;
    db.record_host_capacity("host", 2, &old_sample, later).unwrap();
    assert_eq!(
        available(&db.connect().unwrap(), "host", later).unwrap(),
        0,
        "different boot cannot borrow a fresh challenge"
    );
}

#[test]
fn disk_pressure_preflight_failure_and_enablement_are_independent() {
    let (db, now) = fixture();
    ready(&db, now);
    db.set_host_enabled("host", false).unwrap();
    db.host_capacity_pulse("host", 1, "boot", 10, now).unwrap();
    assert!(!db.get_host("host").unwrap().unwrap().enabled);
    assert_eq!(db.available_host_budget("host").unwrap(), 0);
    db.set_host_enabled("host", true).unwrap();
    assert_eq!(db.available_host_budget("host").unwrap(), 4);
    let mut pressure = report();
    pressure.generation = 3;
    pressure.resources.disk_bytes = DISK_FLOOR_BYTES;
    db.record_host_capacity("host", 1, &pressure, now).unwrap();
    assert_eq!(db.available_host_budget("host").unwrap(), 0);
    pressure = report();
    pressure.generation = 4;
    pressure.preflights_ok = false;
    db.record_host_capacity("host", 1, &pressure, now).unwrap();
    assert_eq!(db.available_host_budget("host").unwrap(), 0);
    assert!(db.get_host("host").unwrap().unwrap().enabled);
}
