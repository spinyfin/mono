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
    assert_eq!(first.hello.features, boss_spoke_stream::outbound::contracts().features);
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
    let one = capacity(connection.next().await);
    connection.receipt(one.sequence).await;
    let two = capacity(connection.next().await);
    assert_eq!((one.sequence, two.sequence), (1, 2));
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
