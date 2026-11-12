use boss_protocol_spoke::v1::{
    self, Command, CommandResult, InventorySnapshot, Record, ResultCode, RunObservation, command, record,
};
use boss_spoke_stream::store::{CommandAdmission, CommandOutbox, SpokeObservationSink, StoreError};
use prost::Message;

use crate::support::*;

#[tokio::test]
async fn capacity_is_admitted_only_after_a_fresh_pulse_and_applied_inventory() {
    let (_dir, db) = work_db();
    db.add_host("host-a", "", 2, &[]).unwrap();
    spoke_ready(&db, "host-a");
    let (sink, _) = stores(db.clone());
    let admission = sink.open(&hello("host-a"), &selection()).await.unwrap();
    let current = session("host-a", admission.generation);
    let mut report = capacity(1);
    report.body = Some(record::Body::Capacity(v1::Capacity {
        generation: 1,
        total_slots: 2,
        available_cpu_millis: 2000,
        available_memory_bytes: 2 * 1024 * 1024 * 1024,
        free_disk_bytes: 8 * 1024 * 1024 * 1024,
        inventory: Some(v1::Inventory::default()),
        ..Default::default()
    }));
    sink.commit(&current, &report.encode_to_vec(), &report).await.unwrap();
    assert_eq!(db.available_host_budget("host-a").unwrap(), 0);
    sink.pulse(&current, &stamp()).await.unwrap();
    let inventory = Record {
        sequence: 2,
        observed: Some(stamp()),
        body: Some(record::Body::Inventory(InventorySnapshot {
            through_record: 1,
            last_page: true,
            ..Default::default()
        })),
        ..Default::default()
    };
    sink.commit(&current, &inventory.encode_to_vec(), &inventory)
        .await
        .unwrap();
    report.sequence = 3;
    if let Some(record::Body::Capacity(capacity)) = &mut report.body {
        capacity.generation = 3;
    }
    sink.commit(&current, &report.encode_to_vec(), &report).await.unwrap();
    assert_eq!(
        db.available_host_budget("host-a").unwrap(),
        0,
        "receipt is not applied inventory"
    );
    for sequence in 1..=2 {
        db.advance_spoke_projection("host-a", "journal", sequence, &[]).unwrap();
    }
    assert_eq!(db.available_host_budget("host-a").unwrap(), 2);
    let owned = db.admit_spoke_assignment("worker", "host-a").unwrap();
    sink.disconnected(&current).await.unwrap();
    assert_eq!(db.available_host_budget("host-a").unwrap(), 0);
    assert!(db.get_host("host-a").unwrap().unwrap().enabled);
    assert_eq!(db.spoke_assignment_for_execution("worker").unwrap().unwrap(), owned);
    let mut reconnect = hello("host-a");
    reconnect.last_record = 3;
    let admission = sink.open(&reconnect, &selection()).await.unwrap();
    let next = session("host-a", admission.generation);
    sink.pulse(&next, &stamp()).await.unwrap();
    sink.commit(&next, &report.encode_to_vec(), &report).await.unwrap();
    assert_eq!(
        db.available_host_budget("host-a").unwrap(),
        0,
        "replayed capacity cannot reopen admission"
    );
}

#[tokio::test]
async fn restart_replays_the_durable_receipt_and_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("work.db");
    let first_generation;
    {
        let db = work_db_at(path.clone());
        let (sink, _) = stores(db);
        let hello = hello("host-a");
        let admission = sink.open(&hello, &selection()).await.unwrap();
        first_generation = admission.generation;
        assert_eq!(admission.received.through, 0);
        let session = session("host-a", admission.generation);
        let record = capacity(1);
        let bytes = record.encode_to_vec();
        assert_eq!(sink.commit(&session, &bytes, &record).await.unwrap().through, 1);
    }
    let db = work_db_at(path);
    let (sink, _) = stores(db);
    let mut hello = hello("host-a");
    hello.last_record = 1;
    let again = sink.open(&hello, &selection()).await.unwrap();
    assert!(again.generation > first_generation);
    assert_eq!(again.received.through, 1);
    let session = session("host-a", again.generation);
    let record = capacity(1);
    let bytes = record.encode_to_vec();
    assert_eq!(sink.commit(&session, &bytes, &record).await.unwrap().through, 1);
}

#[tokio::test]
async fn duplicate_frames_return_the_receipt_without_changing_bytes() {
    let (_dir, db) = work_db();
    spoke_ready(&db, "host-a");
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let run = proto_run(&run_key(&owned));
    let (sink, _) = stores(db);
    let admission = sink.open(&hello("host-a"), &selection()).await.unwrap();
    let session = session("host-a", admission.generation);
    let record = pane(1, Some(run));
    let bytes = b"frame-one".as_slice();
    assert_eq!(sink.commit(&session, bytes, &record).await.unwrap().through, 1);
    assert_eq!(sink.commit(&session, bytes, &record).await.unwrap().through, 1);
    assert!(matches!(
        sink.commit(&session, b"other", &record).await.unwrap_err(),
        StoreError::Denied
    ));
}

#[tokio::test]
async fn pane_captures_are_receipted_without_occupying_ingress() {
    let (_dir, db) = work_db();
    spoke_ready(&db, "host-a");
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let run = proto_run(&run_key(&owned));
    let reader = db.clone();
    let (sink, _) = stores(db);
    let admission = sink.open(&hello("host-a"), &selection()).await.unwrap();
    let session = session("host-a", admission.generation);
    let mut capture = pane(1, Some(run.clone()));
    capture.body = Some(record::Body::Capture(v1::PaneCapture {
        subscription_id: "view".into(),
        text_utf8: vec![b'x'; 60 * 1024],
        truncated: true,
    }));
    let mut frame = v1::SpokeFrame {
        body: Some(v1::spoke_frame::Body::Record(capture.clone())),
    }
    .encode_to_vec();
    frame.extend_from_slice(&[0xa0, 0x06, 0x01]);
    assert!(frame.len() > 60 * 1024);
    assert_eq!(sink.commit(&session, &frame, &capture).await.unwrap().through, 1);
    assert_eq!(
        reader.spoke_ingress_frame("host-a", 1).unwrap().as_deref(),
        Some(&b""[..]),
        "a capture is live display data: receipted as a frameless tombstone"
    );
    assert_eq!(reader.spoke_journal("host-a").unwrap().unwrap().received, 1);
    // A retransmission, even with different bytes, is already receipted.
    let Some(record::Body::Capture(text)) = &mut capture.body else {
        unreachable!()
    };
    text.text_utf8 = b"changed".to_vec();
    let mut changed = v1::SpokeFrame {
        body: Some(v1::spoke_frame::Body::Record(capture.clone())),
    }
    .encode_to_vec();
    changed.extend_from_slice(&[0xa0, 0x06, 0x01]);
    assert_eq!(sink.commit(&session, &changed, &capture).await.unwrap().through, 1);
    // Everything else is stored exactly as received.
    let verbatim = pane(2, Some(run));
    assert_eq!(sink.commit(&session, b"verbatim", &verbatim).await.unwrap().through, 2);
    assert_eq!(reader.spoke_ingress_frame("host-a", 2).unwrap().unwrap(), b"verbatim");
}

#[tokio::test]
async fn wrong_host_or_run_is_denied_and_inventory_members_are_checked() {
    let (_dir, db) = work_db();
    spoke_ready(&db, "host-a");
    spoke_ready(&db, "host-b");
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let foreign = db.reserve_spoke_assignment("exec_2", "host-b").unwrap();
    let run = proto_run(&run_key(&owned));
    let other = proto_run(&run_key(&foreign));
    let (sink, _) = stores(db);
    let admission = sink.open(&hello("host-a"), &selection()).await.unwrap();
    let session = session("host-a", admission.generation);
    let stolen = pane(1, Some(other.clone()));
    assert!(matches!(
        sink.commit(&session, b"stolen", &stolen).await.unwrap_err(),
        StoreError::Denied
    ));
    let mut inventory = capacity(1);
    inventory.body = Some(record::Body::Inventory(InventorySnapshot {
        runs: vec![RunObservation {
            run: Some(other),
            pane: Some(v1::PaneState {
                life: v1::PaneLife::Alive.into(),
                boot_id: "boot".into(),
                ..Default::default()
            }),
        }],
        through_record: 0,
        page: 0,
        last_page: true,
    }));
    assert!(matches!(
        sink.commit(&session, b"inventory", &inventory).await.unwrap_err(),
        StoreError::Denied
    ));
    let owned_pane = pane(1, Some(run));
    assert_eq!(sink.commit(&session, b"owned", &owned_pane).await.unwrap().through, 1);
}

#[tokio::test]
async fn a_fenced_generation_cannot_commit() {
    let (_dir, db) = work_db();
    let (sink, _) = stores(db);
    let first = sink.open(&hello("host-a"), &selection()).await.unwrap();
    let second = sink.open(&hello("host-a"), &selection()).await.unwrap();
    assert!(second.generation > first.generation);
    let old = session("host-a", first.generation);
    let live = session("host-a", second.generation);
    let record = capacity(1);
    assert!(matches!(
        sink.commit(&old, b"one", &record).await.unwrap_err(),
        StoreError::Denied
    ));
    assert_eq!(sink.commit(&live, b"one", &record).await.unwrap().through, 1);
}

#[tokio::test]
async fn a_result_survives_reopen_after_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("work.db");
    let run;
    let encoded_result;
    {
        let db = work_db_at(path.clone());
        spoke_ready(&db, "host-a");
        let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
        run = proto_run(&run_key(&owned));
        let (sink, outbox) = stores(db);
        outbox
            .append("host-a", CommandAdmission::Start, stop_command("c1", run.clone()))
            .await
            .unwrap();
        let admission = sink.open(&hello("host-a"), &selection()).await.unwrap();
        let session = session("host-a", admission.generation);
        let result = CommandResult {
            command_id: "c1".into(),
            code: ResultCode::Ok.into(),
            ..Default::default()
        };
        encoded_result = result.encode_to_vec();
        let record = Record {
            sequence: 1,
            observed: Some(stamp()),
            run: Some(run),
            body: Some(record::Body::Result(result)),
        };
        assert_eq!(sink.commit(&session, b"frame", &record).await.unwrap().through, 1);
    }
    let db = work_db_at(path);
    let (_, outbox) = stores(db.clone());
    let completed = outbox.completed("host-a", "c1").await.unwrap().unwrap();
    assert_eq!(completed.command_id, "c1");
    assert_eq!(completed.encode_to_vec(), encoded_result);
    assert_eq!(db.spoke_journal("host-a").unwrap().unwrap().received, 1);
    assert_eq!(
        db.spoke_ingress_frame("host-a", 1).unwrap().as_deref(),
        Some(&b"frame"[..])
    );
}

#[tokio::test]
async fn outbox_resume_and_reject_are_durable() {
    let (_dir, db) = work_db();
    spoke_ready(&db, "host-a");
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let run = proto_run(&run_key(&owned));
    let (_, outbox) = stores(db);
    let journal = outbox.resume("host-a", None).await.unwrap();
    assert_eq!(journal.received, 0);
    let first = outbox
        .append("host-a", CommandAdmission::Start, stop_command("c1", run.clone()))
        .await
        .unwrap();
    assert_eq!(first.sequence, 1);
    let second = outbox
        .append(
            "host-a",
            CommandAdmission::Start,
            Command {
                command_id: "c2".into(),
                run: Some(run),
                not_after_unix_ms: 4_102_444_800_000,
                body: Some(command::Body::Stop(v1::Stop { grace_ms: 2 })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(second.sequence, 2);
    let rejected = outbox.reject("host-a", 1, "stale").await.unwrap();
    assert_eq!(rejected.command_id, "c1");
    assert_eq!(rejected.code, i32::from(ResultCode::Rejected));
    let next = outbox.next("host-a", 0).await.unwrap().unwrap();
    assert_eq!(next.sequence, 1);
    let moved = Command::decode(next.bytes.as_slice()).unwrap();
    assert_eq!(moved.command_id, "c2");
    assert_eq!(moved.sequence, 1);
}

#[tokio::test]
async fn the_caller_chooses_which_commands_stay_addressable_after_resolve() {
    let (_dir, db) = work_db();
    spoke_ready(&db, "host-a");
    let owned = db.reserve_spoke_assignment("exec_1", "host-a").unwrap();
    let run = proto_run(&run_key(&owned));
    assert!(
        db.resolve_spoke_assignment("exec_1", owned.assignment_generation, "released")
            .unwrap()
    );
    let (_, outbox) = stores(db);
    // Start admission is refused once the assignment resolved.
    assert!(matches!(
        outbox
            .append("host-a", CommandAdmission::Start, stop_command("c1", run.clone()))
            .await
            .unwrap_err(),
        StoreError::Denied
    ));
    // The authorizing caller may still address it for cleanup or reads,
    // whatever the command body is (including a plain Stop).
    let stored = outbox
        .append("host-a", CommandAdmission::Recovery, stop_command("c2", run.clone()))
        .await
        .unwrap();
    assert_eq!(stored.command_id, "c2");
    assert_eq!(stored.sequence, 1);
    // Identical retry is idempotent; a different run is still denied.
    let retried = outbox
        .append("host-a", CommandAdmission::Recovery, stop_command("c2", run.clone()))
        .await
        .unwrap();
    assert_eq!(retried, stored);
    let mut foreign = run;
    foreign.spawn_token = "forged".into();
    assert!(matches!(
        outbox
            .append("host-a", CommandAdmission::Recovery, stop_command("c3", foreign))
            .await
            .unwrap_err(),
        StoreError::Denied
    ));
}
