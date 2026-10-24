mod hub;
use boss_protocol_spoke::v1::{self, command, hub_frame, record, spoke_frame};
use boss_spoke_journal::{Journal, Limits};
use hub::Fixture;

fn capacity(body: spoke_frame::Body) -> v1::Record {
    let spoke_frame::Body::Record(record) = body else {
        panic!("expected journal record")
    };
    assert!(matches!(record.body, Some(record::Body::Capacity(_))));
    record
}

#[tokio::test]
async fn actual_daemon_reconnects_replays_and_recovers_command_receipt_after_sigkill() {
    let mut hub = Fixture::new().await;
    let daemon = hub.start();
    let mut first = hub.connection().await;
    assert_eq!(first.hello.features, boss_spoke_outbound::contracts().features);
    let identity = first.hello.journal_id.clone();
    let boot = first.hello.boot_id.clone();
    first.welcome(0).await;
    let observed = capacity(first.next().await);
    let Some(record::Body::Capacity(value)) = &observed.body else {
        unreachable!()
    };
    assert_eq!(value.total_slots, 0);
    assert!(value.free_disk_bytes > 0);
    assert_eq!(observed.sequence, 1);
    drop(first); // The hub received bytes but lost its receipt.
    let mut second = hub.connection().await;
    assert_eq!(second.hello.journal_id, identity);
    second.welcome(0).await;
    assert_eq!(capacity(second.next().await), observed);
    second.receipt(1).await;
    let command = v1::Command {
        sequence: 1,
        command_id: "one".into(),
        not_after_unix_ms: i64::MAX,
        body: Some(command::Body::Inventory(v1::Empty {})),
        ..Default::default()
    };
    second.send(hub_frame::Body::Command(command.clone())).await;
    assert!(matches!(
        second.next().await,
        spoke_frame::Body::CommandReceipt(v1::Cursor { through: 1, .. })
    ));
    drop(daemon); // SIGKILL: no orderly database close or daemon cleanup.
    drop(second);
    let _restarted = hub.start();
    let mut third = hub.connection().await;
    assert_eq!(third.hello.journal_id, identity);
    assert_eq!(third.hello.boot_id, boot);
    assert_eq!(third.hello.received_commands.as_ref().unwrap().through, 1);
    third.welcome(1).await;
    third.send(hub_frame::Body::Command(command)).await;
    let mut result_count = 0;
    let mut receipt_seen = false;
    for _ in 0..3 {
        match third.next().await {
            spoke_frame::Body::CommandReceipt(cursor) => {
                assert_eq!(cursor.through, 1);
                receipt_seen = true;
            }
            spoke_frame::Body::Record(record) => {
                if let Some(record::Body::Result(result)) = record.body {
                    assert_eq!(result.command_id, "one");
                    assert_eq!(result.code, v1::ResultCode::Rejected as i32);
                    result_count += 1;
                }
                third.receipt(record.sequence).await;
            }
            _ => panic!("unexpected frame"),
        }
        if receipt_seen && result_count == 1 {
            break;
        }
    }
    assert!(receipt_seen);
    assert_eq!(result_count, 1);
    drop(third);
    drop(_restarted);
    let journal = Journal::open(&hub.root(), "host", Limits::default()).unwrap();
    assert_eq!(journal.applied_commands().unwrap(), 1);
    let mut after = 0;
    let mut results = 0;
    while let Some(stored) = journal.next_record(after).unwrap() {
        let record = <v1::Record as prost::Message>::decode(stored.bytes.as_slice()).unwrap();
        results += usize::from(matches!(record.body, Some(record::Body::Result(_))));
        after = stored.sequence;
    }
    assert_eq!(results, 1, "duplicate command never creates another result");
}

#[tokio::test]
async fn actual_daemon_journals_capacity_before_welcome_and_replays_offline_backlog() {
    let mut hub = Fixture::new().await;
    let daemon = hub.start();
    let first = hub.connection().await;
    tokio::time::sleep(std::time::Duration::from_millis(5200)).await;
    drop(daemon);
    drop(first);
    let journal = Journal::open(&hub.root(), "host", Limits::default()).unwrap();
    assert!(
        journal.record_bounds().unwrap().1 >= 2,
        "producer continues before stream is admitted"
    );
    drop(journal);
    let _daemon = hub.start();
    let mut connection = hub.connection().await;
    connection.welcome(0).await;
    // Both backlog records are in flight under the credit window before any receipt.
    let one = capacity(connection.next().await);
    let two = capacity(connection.next().await);
    assert_eq!((one.sequence, two.sequence), (1, 2));
    connection.receipt(two.sequence).await;
    assert!(two.observed.unwrap().awake_ns > one.observed.unwrap().awake_ns);
}

#[tokio::test]
async fn actual_daemon_stops_on_gap_without_advancing_receipt() {
    let mut hub = Fixture::new().await;
    let mut daemon = hub.start();
    let connection = hub.connection().await;
    connection.welcome(u64::MAX).await;
    assert!(daemon.failed().await.contains("replay_gap"));
    let journal = Journal::open(&hub.root(), "host", Limits::default()).unwrap();
    assert!(journal.next_record(0).unwrap().is_some());
}

#[tokio::test]
async fn restored_hub_cannot_skip_compacted_history() {
    use boss_spoke_journal::Admission;
    let mut hub = Fixture::new().await;
    let mut journal = Journal::open(&hub.root(), "host", Limits::default()).unwrap();
    let journal_id = journal.identity().unwrap().1;
    journal
        .append(
            v1::Record {
                observed: Some(v1::Stamp {
                    boot_id: "old-boot".into(),
                    ..Default::default()
                }),
                body: Some(record::Body::Capacity(v1::Capacity {
                    generation: 1,
                    ..Default::default()
                })),
                ..Default::default()
            },
            Admission::Capacity,
        )
        .unwrap();
    journal
        .receipt(&v1::Cursor {
            journal_id: journal_id.clone(),
            through: 1,
        })
        .unwrap();
    journal.compact(8 * 24 * 60 * 60 * 1000).unwrap();
    assert_eq!(journal.record_bounds().unwrap(), (2, 1));
    drop(journal);
    let mut daemon = hub.start();
    let connection = hub.connection().await;
    assert_eq!(connection.hello.first_retained, 2);
    connection.welcome(0).await;
    assert!(daemon.failed().await.contains("replay_gap"));
    let journal = Journal::open(&hub.root(), "host", Limits::default()).unwrap();
    assert_eq!(journal.identity().unwrap().1, journal_id);
    assert_eq!(journal.record_bounds().unwrap().0, 2);
}

#[tokio::test]
async fn actual_daemon_rejects_stale_generation_and_unreceived_commands() {
    let mut hub = Fixture::new().await;
    let mut daemon = hub.start();
    let connection = hub.connection().await;
    connection.welcome(0).await;
    connection
        .outgoing
        .send(Ok(v1::HubFrame {
            connection_generation: 99,
            body: Some(hub_frame::Body::Command(v1::Command {
                sequence: 1,
                command_id: "stale".into(),
                not_after_unix_ms: i64::MAX,
                body: Some(command::Body::Inventory(v1::Empty {})),
                ..Default::default()
            })),
        }))
        .await
        .unwrap();
    assert!(daemon.failed().await.contains("stale stream generation"));
    let journal = Journal::open(&hub.root(), "host", Limits::default()).unwrap();
    assert_eq!(journal.received_commands().unwrap().unwrap().through, 0);
}

#[tokio::test]
async fn pulse_reply_is_current_control_and_does_not_consume_sequence() {
    let mut hub = Fixture::new().await;
    let _daemon = hub.start();
    let mut connection = hub.connection().await;
    connection.welcome(0).await;
    connection
        .send(hub_frame::Body::Pulse(v1::Pulse {
            nonce: vec![9; 16],
            stamp: None,
        }))
        .await;
    loop {
        if let spoke_frame::Body::PulseReply(pulse) = connection.next().await {
            assert_eq!(pulse.nonce, vec![9; 16]);
            assert_eq!(pulse.stamp.unwrap().boot_id, connection.hello.boot_id);
            break;
        }
    }
}

/// Spoke daemon session behaviour, driven over the real transport.
mod flow {
    use crate::hub::{Connection, Fixture};
    use boss_protocol_spoke::v1::{self, command, hub_frame, record, spoke_frame};
    use boss_spoke_journal::{Admission, Journal, Limits};
    use boss_spoke_outbound::SharedJournal;
    use prost::Message;
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    const SPOOL: u64 = 16 * 1024;

    fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    }

    fn stamp() -> anyhow::Result<v1::Stamp> {
        Ok(v1::Stamp {
            boot_id: "boot".into(),
            wall_unix_ms: now_ms(),
            awake_ns: 1,
            elapsed_ns: 1,
        })
    }

    fn filler(pad: usize, time: i64) -> v1::Record {
        v1::Record {
            observed: Some(v1::Stamp {
                boot_id: "boot".into(),
                wall_unix_ms: time,
                ..Default::default()
            }),
            body: Some(record::Body::Capacity(v1::Capacity {
                generation: 1,
                inventory: Some(v1::Inventory {
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    failed_preflights: vec!["p".repeat(pad)],
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn inventory_command() -> v1::Command {
        v1::Command {
            sequence: 1,
            command_id: "one".into(),
            not_after_unix_ms: i64::MAX,
            body: Some(command::Body::Inventory(v1::Empty {})),
            ..Default::default()
        }
    }

    /// Journal with a receipted, week-old prefix (reclaimable by compaction), three
    /// fresh unreceipted records and exactly `remaining` bytes of admission left.
    /// Returns the journal, the length of the old prefix and the unreceipted count.
    fn spool(root: &std::path::Path, remaining: u64) -> (Journal, u64, usize) {
        let mut journal = Journal::open(
            root,
            "host",
            Limits {
                spool_bytes: SPOOL,
                lifecycle_reserve: 1024,
            },
        )
        .unwrap();
        let mut old = 0;
        while journal.bytes().unwrap() < SPOOL / 2 {
            journal.append(filler(200, 1000), Admission::Lifecycle).unwrap();
            old += 1;
        }
        for _ in 0..2 {
            journal.append(filler(200, now_ms()), Admission::Capacity).unwrap();
        }
        // Size one filler so the last one can be tuned to the byte budget.
        let before = journal.bytes().unwrap();
        journal.append(filler(200, now_ms()), Admission::Lifecycle).unwrap();
        let cost = journal.bytes().unwrap() - before;
        let base = cost - 200;
        // Leave one to two fillers' worth above the target, then tune it exactly.
        while SPOOL - journal.bytes().unwrap() >= remaining + 2 * cost {
            journal.append(filler(200, now_ms()), Admission::Lifecycle).unwrap();
        }
        let slack = SPOOL - journal.bytes().unwrap() - remaining;
        journal
            .append(filler((slack - base) as usize, now_ms()), Admission::Lifecycle)
            .unwrap();
        let pending = (journal.record_bounds().unwrap().1 - old) as usize;
        (journal, old, pending)
    }

    fn session(hub: &Fixture, journal: Journal) -> (SharedJournal, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let shared: SharedJournal = Arc::new(Mutex::new(journal));
        let endpoint = hub.endpoint();
        let task = tokio::spawn({
            let journal = Arc::clone(&shared);
            async move { boss_spoke_outbound::connect_once(&journal, endpoint, &[1; 16], &stamp).await }
        });
        (shared, task)
    }

    async fn record_or_receipt(connection: &mut Connection) -> spoke_frame::Body {
        let body = connection.next().await;
        if let spoke_frame::Body::Record(record) = &body {
            connection.receipt(record.sequence).await;
        }
        body
    }

    #[tokio::test]
    async fn full_spool_defers_the_command_result_and_keeps_replaying() {
        let mut hub = Fixture::new().await;
        let command_cost = inventory_command().encode_to_vec().len() as u64 + 128;
        // Room for the command, none for its rejection record.
        let (journal, old, pending) = spool(&hub.root(), command_cost + 8);
        let (_journal, task) = session(&hub, journal);
        let mut connection = hub.connection().await;
        connection.welcome(old).await;
        connection.send(hub_frame::Body::Command(inventory_command())).await;
        let (mut records, mut receipts, mut results) = (0, 0, 0);
        while results == 0 {
            match record_or_receipt(&mut connection).await {
                spoke_frame::Body::CommandReceipt(cursor) => {
                    assert_eq!(cursor.through, 1);
                    receipts += 1;
                }
                spoke_frame::Body::Record(record) => match record.body {
                    Some(record::Body::Result(result)) => {
                        assert_eq!(result.command_id, "one");
                        results += 1;
                    }
                    _ => records += 1,
                },
                other => panic!("unexpected frame {other:?}"),
            }
        }
        // The backlog streamed and was receipted, and the result committed once
        // compaction made room, all on the original session.
        assert_eq!((records, receipts, results), (pending, 1, 1));
        assert!(!task.is_finished(), "a full spool must not end the session");
        task.abort();
    }

    #[tokio::test]
    async fn full_inbox_withholds_the_command_receipt_without_ending_the_session() {
        let mut hub = Fixture::new().await;
        let command_cost = inventory_command().encode_to_vec().len() as u64 + 128;
        let (journal, old, pending) = spool(&hub.root(), command_cost - 40);
        let (shared, task) = session(&hub, journal);
        let mut connection = hub.connection().await;
        connection.welcome(0).await;
        connection.send(hub_frame::Body::Command(inventory_command())).await;
        // Without receipts even the old prefix cannot be reclaimed.
        for _ in 0..old as usize + pending {
            assert!(matches!(connection.next().await, spoke_frame::Body::Record(_)));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(300), connection.incoming.message())
                .await
                .is_err()
        );
        assert_eq!(shared.lock().unwrap().received_commands().unwrap().unwrap().through, 0);
        // Free the old prefix, without resending the command or restarting.
        connection.receipt(old + pending as u64).await;
        let (mut receipt, mut result) = (false, false);
        while !receipt || !result {
            match record_or_receipt(&mut connection).await {
                spoke_frame::Body::CommandReceipt(cursor) => {
                    assert_eq!(cursor.through, 1);
                    receipt = true;
                }
                spoke_frame::Body::Record(record) => {
                    let Some(record::Body::Result(value)) = record.body else {
                        panic!("expected result");
                    };
                    assert_eq!(value.command_id, "one");
                    result = true;
                }
                other => panic!("unexpected frame {other:?}"),
            }
        }
        assert_eq!(shared.lock().unwrap().received_commands().unwrap().unwrap().through, 1);
        assert_eq!(shared.lock().unwrap().applied_commands().unwrap(), 1);
        assert!(!task.is_finished());
        task.abort();
    }

    #[tokio::test]
    async fn hub_resource_exhaustion_is_retried_by_the_same_daemon() {
        let mut hub = Fixture::refusing(2).await;
        let mut daemon = hub.start();
        let mut connection = hub.connection().await;
        let identity = connection.hello.journal_id.clone();
        connection.welcome(0).await;
        let spoke_frame::Body::Record(record) = connection.next().await else {
            panic!("expected the journaled capacity record");
        };
        assert_eq!(record.sequence, 1);
        assert!(daemon.running(), "shedding must not stop the daemon");
        assert_eq!(connection.hello.journal_id, identity);
    }

    #[tokio::test]
    async fn saturated_replay_keeps_reading_commands_receipts_and_pulses() {
        let mut hub = Fixture::new().await;
        let mut journal = Journal::open(&hub.root(), "host", Limits::default()).unwrap();
        for _ in 0..2000 {
            journal.append(filler(200, now_ms()), Admission::Capacity).unwrap();
        }
        let (shared, task) = session(&hub, journal);
        let mut connection = hub.connection().await;
        connection.welcome(0).await;
        // The server's small HTTP/2 window stops replay while it is not reading.
        tokio::time::sleep(Duration::from_millis(300)).await;
        connection.send(hub_frame::Body::Command(inventory_command())).await;
        connection
            .send(hub_frame::Body::Pulse(v1::Pulse {
                nonce: vec![9; 16],
                stamp: None,
            }))
            .await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while shared.lock().unwrap().received_commands().unwrap().unwrap().through == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("inbound command must commit while outbound transport is blocked");
        let (mut records, mut pulses, mut receipt) = (0, 0, false);
        tokio::time::timeout(Duration::from_secs(10), async {
            while records < 2001 || pulses < 21 || !receipt {
                match record_or_receipt(&mut connection).await {
                    spoke_frame::Body::Record(record) => {
                        records += 1;
                        assert_eq!(record.sequence, records);
                        if records % 100 == 0 {
                            connection
                                .send(hub_frame::Body::Pulse(v1::Pulse {
                                    nonce: vec![9; 16],
                                    stamp: None,
                                }))
                                .await;
                        }
                    }
                    spoke_frame::Body::PulseReply(pulse) => {
                        assert_eq!(pulse.nonce, vec![9; 16]);
                        pulses += 1;
                    }
                    spoke_frame::Body::CommandReceipt(cursor) => {
                        assert_eq!(cursor.through, 1);
                        receipt = true;
                    }
                    other => panic!("unexpected frame {other:?}"),
                }
            }
        })
        .await
        .expect("replay, receipts and pulse replies must keep flowing");
        assert!(!task.is_finished());
        task.abort();
    }
}
