use boss_protocol_spoke::v1::{self, command, record};
use boss_spoke_journal::{Admission, Fault, Journal, Limits};
use prost::Message;

fn open(root: &std::path::Path) -> Journal {
    Journal::open(root, "host", Limits::default()).unwrap()
}
fn stamp(time: i64) -> v1::Stamp {
    v1::Stamp {
        boot_id: "boot".into(),
        wall_unix_ms: time,
        ..Default::default()
    }
}
fn contract() -> v1::Hello {
    v1::Hello {
        max_epoch: 1,
        features: vec![v1::Feature {
            name: "journal".into(),
            revision: 1,
        }],
        ..Default::default()
    }
}
fn capacity(time: i64) -> v1::Record {
    v1::Record {
        observed: Some(stamp(time)),
        body: Some(record::Body::Capacity(v1::Capacity {
            generation: 1,
            ..Default::default()
        })),
        ..Default::default()
    }
}
fn welcome(journal: &Journal, through: u64) -> v1::Welcome {
    v1::Welcome {
        hub_id: "hub".into(),
        authority_generation: "authority".into(),
        command_journal_id: "commands".into(),
        received_records: Some(v1::Cursor {
            journal_id: journal.identity().unwrap().1,
            through,
        }),
        ..Default::default()
    }
}
fn command(sequence: u64, id: &str) -> Vec<u8> {
    v1::Command {
        sequence,
        command_id: id.into(),
        not_after_unix_ms: 1000,
        body: Some(command::Body::Inventory(v1::Empty {})),
        ..Default::default()
    }
    .encode_to_vec()
}

#[test]
fn lock_identity_migration_and_sequence_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let mut journal = open(&root);
    assert!(Journal::open(&root, "host", Limits::default()).is_err());
    let id = journal.identity().unwrap();
    assert_eq!(
        journal.append(capacity(1), Admission::Capacity, &contract()).unwrap(),
        1
    );
    let raw = journal.next_record(0).unwrap().unwrap().bytes;
    drop(journal);
    let mut journal = open(&root);
    assert_eq!(journal.identity().unwrap(), id);
    assert_eq!(journal.next_record(0).unwrap().unwrap().bytes, raw);
    assert_eq!(
        journal.append(capacity(2), Admission::Capacity, &contract()).unwrap(),
        2
    );
    drop(journal);
    assert!(Journal::open(&root, "another-host", Limits::default()).is_err());
    let db = rusqlite::Connection::open(root.join("journal.db")).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    assert!(
        Journal::open(&root, "host", Limits::default())
            .err()
            .expect("new schema refused")
            .to_string()
            .contains("newer")
    );
}

#[test]
fn rollback_replays_committed_prefix_without_allocating_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let mut journal = open(&root);
    journal.append(capacity(1), Admission::Capacity, &contract()).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(root.join("journal.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE; UPDATE identity SET next_record=99;")
        .unwrap();
    drop(db); // An interrupted transaction must not consume sequence numbers.
    let mut journal = open(&root);
    assert_eq!(
        journal.append(capacity(2), Admission::Capacity, &contract()).unwrap(),
        2
    );
}

#[test]
fn missing_suffix_and_corrupt_record_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let mut journal = open(&root);
    journal.append(capacity(1), Admission::Capacity, &contract()).unwrap();
    journal.append(capacity(2), Admission::Capacity, &contract()).unwrap();
    drop(journal);
    let db = rusqlite::Connection::open(root.join("journal.db")).unwrap();
    db.execute("DELETE FROM records WHERE sequence=2", []).unwrap();
    db.execute("UPDATE records SET digest=zeroblob(32) WHERE sequence=1", [])
        .unwrap();
    drop(db);
    let journal = open(&root);
    assert!(journal.next_record(0).err().unwrap().to_string().contains("digest"));
    assert!(journal.next_record(1).err().unwrap().to_string().contains("replay_gap"));
}

#[test]
fn inbox_deduplicates_raw_unknown_fields_and_commits_result_with_application() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let mut journal = open(&root);
    journal.welcome(&welcome(&journal, 0)).unwrap();
    let mut raw = command(1, "one");
    raw.extend_from_slice(&[0xa0, 0x06, 0x07]); // Unknown varint field 100.
    assert_eq!(journal.receive_command(&raw).unwrap(), 1);
    assert_eq!(journal.receive_command(&raw).unwrap(), 1);
    assert!(journal.receive_command(&command(1, "one")).is_err());
    assert!(journal.receive_command(&command(3, "three")).is_err());
    assert!(journal.receive_command(&command(2, "one")).is_err());
    drop(journal); // Crash boundary: receipt committed, application not started.
    let mut journal = open(&root);
    assert_eq!(journal.received_commands().unwrap().unwrap().through, 1);
    assert_eq!(journal.applied_commands().unwrap(), 0);
    assert!(journal.reject_pending(&stamp(2000), &contract()).unwrap());
    assert!(!journal.reject_pending(&stamp(2000), &contract()).unwrap());
    let result = v1::Record::decode(journal.next_record(0).unwrap().unwrap().bytes.as_slice()).unwrap();
    let Some(record::Body::Result(result)) = result.body else {
        panic!("missing result")
    };
    assert_eq!(result.reason, "command expired");
    assert_eq!(result.code, v1::ResultCode::Rejected as i32);
    assert_eq!(journal.applied_commands().unwrap(), 1);
    drop(journal);
    let mut journal = open(&root);
    assert_eq!(journal.receive_command(&raw).unwrap(), 1);
    assert!(!journal.reject_pending(&stamp(2001), &contract()).unwrap());
    assert_eq!(journal.record_bounds().unwrap(), (1, 1));
}

#[test]
fn receipt_retention_and_gap_rules_do_not_silently_reset_identity() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(&dir.path().join("state"));
    for _ in 0..3 {
        journal.append(capacity(0), Admission::Capacity, &contract()).unwrap();
    }
    journal.welcome(&welcome(&journal, 2)).unwrap();
    journal.compact(1000).unwrap();
    assert_eq!(journal.record_bounds().unwrap(), (1, 3));
    journal.welcome(&welcome(&journal, 0)).unwrap(); // Retained rollback is replayable.
    journal.compact(8 * 24 * 60 * 60 * 1000).unwrap();
    assert_eq!(
        journal.record_bounds().unwrap(),
        (1, 3),
        "requested replay stays pinned"
    );
    journal
        .receipt(welcome(&journal, 2).received_records.as_ref().unwrap())
        .unwrap();
    journal.compact(8 * 24 * 60 * 60 * 1000).unwrap();
    assert_eq!(journal.record_bounds().unwrap(), (3, 3)); // Unacked record is kept.
    for through in [0, 4] {
        let error = journal.welcome(&welcome(&journal, through)).unwrap_err();
        assert!(matches!(error.downcast_ref(), Some(Fault::ReplayGap(_))));
    }
    let mut changed = welcome(&journal, 2);
    changed.authority_generation = "restored".into();
    assert!(journal.welcome(&changed).is_err());
    changed = welcome(&journal, 2);
    changed.command_journal_id = "reinstalled".into();
    assert!(journal.welcome(&changed).is_err());
    changed = welcome(&journal, 2);
    changed.received_records.as_mut().unwrap().journal_id = "missing".into();
    assert!(journal.welcome(&changed).is_err());
}

#[test]
fn bounded_spool_preserves_reserved_lifecycle_space_without_consuming_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Journal::open(
        &dir.path().join("state"),
        "host",
        Limits {
            spool_bytes: 4096,
            lifecycle_reserve: 1024,
        },
    )
    .unwrap();
    while journal.append(capacity(1), Admission::Capacity, &contract()).is_ok() {}
    let before = journal.record_bounds().unwrap();
    assert!(journal.append(capacity(1), Admission::Bulk, &contract()).is_err());
    assert!(journal.append(capacity(1), Admission::Capture, &contract()).is_err());
    assert_eq!(journal.record_bounds().unwrap(), before);
    assert_eq!(
        journal.append(capacity(1), Admission::Lifecycle, &contract()).unwrap(),
        before.1 + 1
    );
    while journal.append(capacity(1), Admission::Lifecycle, &contract()).is_ok() {}
    assert!(journal.bytes().unwrap() <= 4096);
}
