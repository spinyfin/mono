mod hub;
mod upgrade_recovery;
use boss_protocol_spoke::v1::{self, command, hub_frame, record, spoke_frame};
use boss_spoke_journal::{Journal, Limits};
use hub::Fixture;

async fn capacity(connection: &mut hub::Connection) -> v1::Record {
    loop {
        let spoke_frame::Body::Record(record) = connection.next().await else {
            panic!("expected journal record")
        };
        if matches!(record.body, Some(record::Body::Capacity(_))) {
            return record;
        }
        assert!(matches!(record.body, Some(record::Body::Inventory(_))));
    }
}

fn journal_lock_still_held(error: &anyhow::Error) -> bool {
    let text = error.to_string();
    text.contains("lock is held")
        || text.contains("Resource temporarily unavailable")
        || text.contains("temporarily unavailable")
        || text.contains("WouldBlock")
        || text.contains("already locked")
        || error.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
            )
        })
}

/// Opens the journal once the stopped daemon's exclusive lock is released.
/// The lock drops when the kernel finishes tearing the killed process down,
/// which on a loaded host can trail the parent's `wait` by a moment.
fn open_after_daemon_exit(root: &std::path::Path) -> Journal {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match Journal::open(root, "host", Limits::default()) {
            Ok(journal) => return journal,
            Err(error) if journal_lock_still_held(&error) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => panic!("journal open failed: {error}"),
        }
    }
}

#[tokio::test]
async fn actual_daemon_drains_pending_hook_without_another_peer_connection() {
    use std::os::unix::fs::PermissionsExt;
    let mut hub = Fixture::new().await;
    let pending = hub.dir.path().join("pending");
    std::fs::create_dir(&pending).unwrap();
    std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = pending.join("00000000000000000001-0123456789abcdef0123456789abcdef.json");
    std::fs::write(
        &source,
        serde_json::to_vec(&serde_json::json!({
            "_boss_run_id":"run", "_boss_producer_id":"0123456789abcdef0123456789abcdef", "_boss_producer_sequence":1,
            "session_id":"session", "hook_event_name":"Stop", "stop_hook_active":false
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut journal = Journal::open(&hub.root(), "host", Limits::default()).unwrap();
    journal
        .save_execution(
            "run",
            "token",
            &serde_json::to_vec(&serde_json::json!({
                "execution_id":"execution", "run_id":"run", "spawn_token":"token", "assignment_generation":1,
                "lease_id":"lease", "workspace_id":"workspace", "session":"session", "phase":"Alive",
                "boot_id":"old-boot", "driver_id":"claude",
                "observation":{"workspace":hub.dir.path(), "pending":pending, "file":null, "pid_start":null}
            }))
            .unwrap(),
        )
        .unwrap();
    let daemon = hub.start_after_journal(journal);
    let mut connection = hub.connection().await;
    connection.welcome(0).await;
    loop {
        if let spoke_frame::Body::Record(record) = connection.next().await {
            connection.receipt(record.sequence).await;
            if matches!(record.body, Some(record::Body::Event(_))) {
                break;
            }
        }
    }
    assert!(!source.exists(), "daemon independently drained the pending event");
    drop(daemon);
    let journal = open_after_daemon_exit(&hub.root());
    assert!(
        journal
            .source(
                &v1::Run {
                    execution_id: "execution".into(),
                    run_id: "run".into(),
                    spawn_token: "token".into(),
                    assignment_generation: 1
                },
                "hook:0123456789abcdef0123456789abcdef:1"
            )
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn actual_daemon_reconnects_replays_and_recovers_command_receipt_after_sigkill() {
    let mut hub = Fixture::new().await;
    let daemon = hub.start();
    let mut first = hub.connection().await;
    assert_eq!(first.hello.features, boss_spoke_outbound::executor_contracts().features);
    assert!(
        first.hello.features.iter().any(|feature| {
            feature.name == boss_protocol_spoke::negotiate::feature::UPGRADE && feature.revision == 1
        })
    );
    let identity = first.hello.journal_id.clone();
    let boot = first.hello.boot_id.clone();
    first.welcome(0).await;
    let observed = capacity(&mut first).await;
    let Some(record::Body::Capacity(value)) = &observed.body else {
        unreachable!()
    };
    assert!(value.total_slots > 0);
    assert!(value.available_memory_bytes > 0);
    assert!(value.available_cpu_millis > 0);
    assert!(value.free_disk_bytes > 0);
    assert_eq!(observed.sequence, 1);
    drop(first); // The hub received bytes but lost its receipt.
    let mut second = hub.connection().await;
    assert_eq!(second.hello.journal_id, identity);
    second.welcome(0).await;
    assert_eq!(capacity(&mut second).await, observed);
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
    let journal = open_after_daemon_exit(&hub.root());
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
async fn actual_daemon_applies_an_upgrade_offer_and_reports_the_running_release() {
    let mut hub = Fixture::new().await;
    std::fs::write(
        hub.dir.path().join("install.json"),
        serde_json::json!({
            "schema": 1,
            "release_id": "1.0.1",
            "platform": "macos-arm64",
            "user": "boss-worker",
            "host_id": "host",
            "hub_url": "https://hub.example:7443",
            "package_origin": "https://github.com/spinyfin/mono/releases"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        hub.dir.path().join("upgrade-status.json"),
        serde_json::json!({
            "schema": 1,
            "command_id": "prev",
            "release_id": "1.0.1",
            "result": "refused",
            "error": "unresolved_rollback",
            "unresolved_result": "rollback_failed",
            "unresolved_error": "previous did not restart",
            "unresolved_release_id": "1.0.2",
            "running_release_id": "1.0.1"
        })
        .to_string(),
    )
    .unwrap();
    let daemon = hub.start();
    let mut connection = hub.connection().await;
    assert!(
        connection.hello.features.iter().any(|feature| {
            feature.name == boss_protocol_spoke::negotiate::feature::UPGRADE && feature.revision == 1
        })
    );
    connection.welcome(0).await;
    loop {
        if let spoke_frame::Body::Record(record) = connection.next().await {
            connection.receipt(record.sequence).await;
            if let Some(record::Body::Capacity(capacity)) = record.body
                && let Some(inventory) = capacity.inventory
            {
                assert_eq!(inventory.release_id, "1.0.1");
                assert_eq!(inventory.last_upgrade_command_id, "prev");
                assert_eq!(inventory.last_upgrade_result, "refused");
                assert_eq!(inventory.unresolved_upgrade_result, "rollback_failed");
                assert_eq!(inventory.unresolved_upgrade_error, "previous did not restart");
                assert_eq!(inventory.unresolved_upgrade_release_id, "1.0.2");
                break;
            }
        }
    }
    let command = v1::Command {
        sequence: 1,
        command_id: "upgrade-one".into(),
        not_after_unix_ms: i64::MAX,
        body: Some(command::Body::Upgrade(v1::Upgrade {
            release_id: "1.0.2".into(),
            manifest_url: "https://github.com/spinyfin/mono/releases/download/x/manifest.json".into(),
            manifest_sha256: vec![0xab; 32],
        })),
        ..Default::default()
    };
    connection.send(hub_frame::Body::Command(command)).await;
    let mut accepted = false;
    for _ in 0..8 {
        match connection.next().await {
            spoke_frame::Body::CommandReceipt(cursor) => assert_eq!(cursor.through, 1),
            spoke_frame::Body::Record(record) => {
                if let Some(record::Body::Result(result)) = &record.body {
                    assert_eq!(result.command_id, "upgrade-one");
                    assert_eq!(result.code, v1::ResultCode::Ok as i32);
                    accepted = true;
                }
                connection.receipt(record.sequence).await;
            }
            _ => {}
        }
        if accepted {
            break;
        }
    }
    assert!(accepted, "the daemon must accept Upgrade on the management contract");
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(hub.root().join("update-request").join("request.json")).unwrap())
            .unwrap();
    assert_eq!(request["release_id"], "1.0.2");
    assert_eq!(request["command_id"], "upgrade-one");
    drop(daemon);
}

fn management_features() -> Vec<v1::Feature> {
    boss_spoke_outbound::contracts().features
}

async fn send_upgrade(connection: &mut hub::Connection, command_id: &str) -> bool {
    let command = v1::Command {
        sequence: 1,
        command_id: command_id.into(),
        not_after_unix_ms: i64::MAX,
        body: Some(command::Body::Upgrade(v1::Upgrade {
            release_id: "1.0.2".into(),
            manifest_url: "https://github.com/spinyfin/mono/releases/download/x/manifest.json".into(),
            manifest_sha256: vec![0xab; 32],
        })),
        ..Default::default()
    };
    connection.send(hub_frame::Body::Command(command)).await;
    let mut accepted = false;
    for _ in 0..8 {
        match connection.next().await {
            spoke_frame::Body::CommandReceipt(cursor) => assert_eq!(cursor.through, 1),
            spoke_frame::Body::Record(record) => {
                if let Some(record::Body::Result(result)) = &record.body {
                    assert_eq!(result.command_id, command_id);
                    assert_eq!(result.code, v1::ResultCode::Ok as i32);
                    accepted = true;
                }
                connection.receipt(record.sequence).await;
            }
            _ => {}
        }
        if accepted {
            break;
        }
    }
    accepted
}

#[tokio::test]
async fn actual_daemon_accepts_upgrade_when_welcome_selects_only_journal_and_upgrade() {
    let mut hub = Fixture::new().await;
    let daemon = hub.start();
    let mut connection = hub.connection().await;
    assert_eq!(
        connection.hello.features,
        boss_spoke_outbound::executor_contracts().features
    );
    connection.welcome_features(0, management_features()).await;
    loop {
        if let spoke_frame::Body::Record(record) = connection.next().await {
            connection.receipt(record.sequence).await;
            if matches!(record.body, Some(record::Body::Capacity(_))) {
                break;
            }
        }
    }
    assert!(
        send_upgrade(&mut connection, "upgrade-mgmt").await,
        "production daemon must accept Upgrade after a journal+upgrade Welcome"
    );
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(hub.root().join("update-request").join("request.json")).unwrap())
            .unwrap();
    assert_eq!(request["release_id"], "1.0.2");
    assert_eq!(request["command_id"], "upgrade-mgmt");
    drop(daemon);
}

#[tokio::test]
async fn actual_daemon_retries_management_hello_after_disjoint_execution_epoch() {
    let mut hub = Fixture::new().await;
    let daemon = hub.start();
    let first = hub.connection().await;
    assert_eq!(first.hello.features, boss_spoke_outbound::executor_contracts().features);
    first
        .reject(
            boss_protocol_spoke::negotiate::reject_code::NO_COMMON_EPOCH,
            "execution epoch 2 does not overlap",
        )
        .await;
    drop(first);
    let mut second = hub.connection().await;
    assert_eq!(second.hello.features, management_features());
    assert_eq!(second.hello.min_epoch, 1);
    assert_eq!(second.hello.max_epoch, 1);
    second.welcome_features(0, management_features()).await;
    loop {
        if let spoke_frame::Body::Record(record) = second.next().await {
            second.receipt(record.sequence).await;
            if matches!(record.body, Some(record::Body::Capacity(_))) {
                break;
            }
        }
    }
    assert!(
        send_upgrade(&mut second, "upgrade-epoch").await,
        "management reconnect must still accept Upgrade"
    );
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(hub.root().join("update-request").join("request.json")).unwrap())
            .unwrap();
    assert_eq!(request["command_id"], "upgrade-epoch");
    drop(daemon);
}

#[tokio::test]
async fn actual_daemon_refuses_a_second_upgrade_offer_while_one_is_pending() {
    let mut hub = Fixture::new().await;
    let daemon = hub.start();
    let mut connection = hub.connection().await;
    connection.welcome_features(0, management_features()).await;
    loop {
        if let spoke_frame::Body::Record(record) = connection.next().await {
            connection.receipt(record.sequence).await;
            if matches!(record.body, Some(record::Body::Capacity(_))) {
                break;
            }
        }
    }
    assert!(send_upgrade(&mut connection, "upgrade-a").await);
    let second = v1::Command {
        sequence: 2,
        command_id: "upgrade-b".into(),
        not_after_unix_ms: i64::MAX,
        body: Some(command::Body::Upgrade(v1::Upgrade {
            release_id: "1.0.3".into(),
            manifest_url: "https://github.com/spinyfin/mono/releases/download/x/manifest.json".into(),
            manifest_sha256: vec![0xcd; 32],
        })),
        ..Default::default()
    };
    connection.send(hub_frame::Body::Command(second)).await;
    let mut busy = false;
    for _ in 0..8 {
        match connection.next().await {
            spoke_frame::Body::CommandReceipt(_) => {}
            spoke_frame::Body::Record(record) => {
                if let Some(record::Body::Result(result)) = &record.body
                    && result.command_id == "upgrade-b"
                {
                    assert_eq!(result.code, v1::ResultCode::Rejected as i32);
                    assert!(result.reason.contains("upgrade_busy"), "{}", result.reason);
                    busy = true;
                }
                connection.receipt(record.sequence).await;
            }
            _ => {}
        }
        if busy {
            break;
        }
    }
    assert!(busy, "overlapping Upgrade must be refused as busy");
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(hub.root().join("update-request").join("request.json")).unwrap())
            .unwrap();
    assert_eq!(request["command_id"], "upgrade-a");
    drop(daemon);
}

#[tokio::test]
async fn actual_daemon_journals_capacity_before_welcome_and_replays_offline_backlog() {
    let mut hub = Fixture::new().await;
    let daemon = hub.start();
    let first = hub.connection().await;
    // The daemon journals capacity every 5s from its start (first tick
    // immediate). Wait well past the second tick so a slow sample/append on a
    // loaded host still lands before the daemon is stopped.
    tokio::time::sleep(std::time::Duration::from_secs(12)).await;
    drop(daemon);
    drop(first);
    let journal = open_after_daemon_exit(&hub.root());
    assert!(
        journal.record_bounds().unwrap().1 >= 2,
        "producer continues before stream is admitted"
    );
    let _daemon = hub.start_after_journal(journal);
    let mut connection = hub.connection().await;
    connection.welcome(0).await;
    // Both backlog records are in flight under the credit window before any receipt.
    let one = capacity(&mut connection).await;
    let two = capacity(&mut connection).await;
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
    let journal = open_after_daemon_exit(&hub.root());
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
    let mut daemon = hub.start_after_journal(journal);
    let connection = hub.connection().await;
    assert_eq!(connection.hello.first_retained, 2);
    connection.welcome(0).await;
    assert!(daemon.failed().await.contains("replay_gap"));
    let journal = open_after_daemon_exit(&hub.root());
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
    let journal = open_after_daemon_exit(&hub.root());
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
        connection.welcome(0).await;
        let spoke_frame::Body::Record(record) = connection.next().await else {
            panic!("expected the journaled capacity record");
        };
        assert_eq!(record.sequence, 1);
        assert!(daemon.running(), "shedding must not stop the daemon");
        let refused = hub.refused.lock().unwrap().clone();
        assert_eq!(refused.len(), 2, "both refusals were observed");
        assert!(
            refused.iter().all(|id| *id == connection.hello.journal_id),
            "every retry presents the same journal"
        );
    }

    #[tokio::test]
    async fn hub_admission_races_are_retried_by_the_same_daemon() {
        let mut hub = Fixture::shedding(vec![tonic::Code::Aborted, tonic::Code::AlreadyExists]).await;
        let mut daemon = hub.start();
        let mut connection = hub.connection().await;
        connection.welcome(0).await;
        let spoke_frame::Body::Record(record) = connection.next().await else {
            panic!("expected the journaled capacity record");
        };
        assert_eq!(record.sequence, 1);
        assert!(daemon.running(), "admission races must not stop the daemon");
        assert_eq!(hub.refused.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn record_larger_than_the_send_credit_is_still_sent_alone() {
        let mut hub = Fixture::new().await;
        let mut journal = Journal::open(&hub.root(), "host", Limits::default()).unwrap();
        journal.append(filler(4096, now_ms()), Admission::Capacity).unwrap();
        let (_shared, task) = session(&hub, journal);
        let mut connection = hub.connection().await;
        connection
            .send(hub_frame::Body::Welcome(v1::Welcome {
                max_inflight_bytes: 1024,
                ..connection.welcome_body(0)
            }))
            .await;
        let spoke_frame::Body::Record(record) = connection.next().await else {
            panic!("expected the oversized record");
        };
        assert_eq!(record.sequence, 1);
        assert!(!task.is_finished(), "an oversized record must not end the session");
        task.abort();
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
        // Merge-queue hosts run this crate's daemon cases in the same binary
        // while the rest of //... saturates the machine. Isolated this
        // handshake finishes in well under a second; 30s is headroom for
        // the inbound commit, 60s for draining the 2000-record replay.
        tokio::time::timeout(Duration::from_secs(30), async {
            while shared.lock().unwrap().received_commands().unwrap().unwrap().through == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("inbound command must commit while outbound transport is blocked");
        let (mut records, mut pulses, mut receipt) = (0, 0, false);
        tokio::time::timeout(Duration::from_secs(60), async {
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

#[tokio::test]
async fn actual_daemon_dispatches_workspace_command_and_replays_result() {
    use std::os::unix::fs::PermissionsExt;
    let mut hub = Fixture::new().await;
    let cube = hub.dir.path().join("cube-fixture");
    std::fs::write(
        &cube,
        r#"#!/bin/sh
[ "$*" = "--json repo list" ] || exit 9
printf '%s\n' '{"repos":[]}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&cube, std::fs::Permissions::from_mode(0o700)).unwrap();
    let daemon = hub.start_with_cube(&cube);
    let mut connection = hub.connection().await;
    connection.welcome(0).await;
    let request = v1::Command {
        sequence: 1,
        command_id: "list-repos".into(),
        not_after_unix_ms: i64::MAX,
        body: Some(command::Body::Workspace(v1::WorkspaceOp {
            kind: v1::WorkspaceKind::ListRepos.into(),
            ..Default::default()
        })),
        ..Default::default()
    };
    connection.send(hub_frame::Body::Command(request)).await;
    let result_record = loop {
        if let spoke_frame::Body::Record(record) = connection.next().await {
            if matches!(record.body, Some(record::Body::Result(_))) {
                break record;
            }
            connection.receipt(record.sequence).await;
        }
    };
    let Some(record::Body::Result(result)) = &result_record.body else {
        unreachable!()
    };
    assert_eq!(result.code, v1::ResultCode::Ok as i32);
    let Some(v1::command_result::Value::Workspace(value)) = &result.value else {
        panic!("missing workspace result")
    };
    assert_eq!(value.cube_result_json, b"[]");
    drop(connection);
    drop(daemon);
    let _daemon = hub.start_with_cube(&cube);
    let mut connection = hub.connection().await;
    connection.welcome(result_record.sequence - 1).await;
    assert_eq!(connection.next().await, spoke_frame::Body::Record(result_record));
}

#[tokio::test]
async fn actual_daemon_offers_reset_release_and_dispatches_it() {
    use std::os::unix::fs::PermissionsExt;
    let mut hub = Fixture::new().await;
    let cube = hub.dir.path().join("cube-fixture");
    let seen = hub.dir.path().join("reset-release.seen");
    std::fs::write(
        &cube,
        format!(
            r#"#!/bin/sh
if [ "$*" = "--json workspace list" ]; then
printf '%s\n' '{{"workspaces":[{{"repo":"repo","workspace_id":"worker","workspace_path":"/tmp/unused","state":"leased","lease_id":"lease","task":"boss-spoke:exec1:run1:1:token"}}]}}'
exit 0
fi
if [ "$*" = "--json workspace release --lease lease --force-reset" ]; then
touch '{seen}'
printf '%s\n' '{{}}'
exit 0
fi
exit 9
"#,
            seen = seen.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cube, std::fs::Permissions::from_mode(0o700)).unwrap();
    let daemon = hub.start_with_cube(&cube);
    let mut connection = hub.connection().await;
    assert_eq!(
        connection.hello.features,
        boss_spoke_outbound::executor_contracts().features
    );
    assert!(connection.hello.features.iter().any(|feature| {
        feature.name == boss_protocol_spoke::negotiate::feature::WORKSPACE_RESET_RELEASE && feature.revision == 1
    }));
    connection.welcome(0).await;
    let request = v1::Command {
        sequence: 1,
        command_id: "reset-release".into(),
        not_after_unix_ms: i64::MAX,
        run: Some(v1::Run {
            execution_id: "exec1".into(),
            run_id: "run1".into(),
            spawn_token: "token".into(),
            assignment_generation: 1,
        }),
        body: Some(command::Body::Workspace(v1::WorkspaceOp {
            kind: v1::WorkspaceKind::ReleaseWithReset.into(),
            repo_id: "repo".into(),
            workspace_id: "worker".into(),
            lease_id: "lease".into(),
            ..Default::default()
        })),
    };
    connection.send(hub_frame::Body::Command(request)).await;
    let result_record = loop {
        if let spoke_frame::Body::Record(record) = connection.next().await {
            if matches!(record.body, Some(record::Body::Result(_))) {
                break record;
            }
            connection.receipt(record.sequence).await;
        }
    };
    let Some(record::Body::Result(result)) = &result_record.body else {
        unreachable!()
    };
    assert_eq!(result.code, v1::ResultCode::Ok as i32, "{}", result.reason);
    assert!(seen.is_file(), "reset release must reach cube --force-reset");
    drop(connection);
    drop(daemon);
}

#[tokio::test]
async fn actual_daemon_bounds_large_result_and_replays_diagnostic() {
    use std::os::unix::fs::PermissionsExt;
    let mut hub = Fixture::new().await;
    let cube = hub.dir.path().join("cube-fixture");
    std::fs::write(
        &cube,
        r#"#!/bin/sh
[ "$*" = "--json repo list" ] || exit 9
printf '%s\n' '{"repos":[]}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&cube, std::fs::Permissions::from_mode(0o700)).unwrap();
    let payload = format!(
        r#"{{"repos":[{{"repo":"r","origin":"{}","main_branch":"main","workspace_root":"/unused","workspace_prefix":"worker"}}]}}"#,
        "x".repeat(24000)
    );
    std::fs::write(&cube, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", payload)).unwrap();
    let daemon = hub.start_with_cube(&cube);
    let mut connection = hub.connection().await;
    connection
        .send(hub_frame::Body::Welcome(v1::Welcome {
            max_frame_bytes: 8192,
            max_inflight_bytes: 4096,
            ..connection.welcome_body(0)
        }))
        .await;
    let request = v1::Command {
        sequence: 1,
        command_id: "list-repos".into(),
        not_after_unix_ms: i64::MAX,
        body: Some(command::Body::Workspace(v1::WorkspaceOp {
            kind: v1::WorkspaceKind::ListRepos.into(),
            ..Default::default()
        })),
        ..Default::default()
    };
    connection.send(hub_frame::Body::Command(request)).await;
    let result_record = loop {
        if let spoke_frame::Body::Record(record) = connection.next().await {
            if matches!(record.body, Some(record::Body::Result(_))) {
                break record;
            }
            connection.receipt(record.sequence).await;
        }
    };
    let Some(record::Body::Result(result)) = &result_record.body else {
        unreachable!()
    };
    assert_eq!(result.code, v1::ResultCode::Rejected as i32);
    assert!(result.value.is_none());
    assert!(result.reason.contains("exceeds negotiated"));
    assert!(prost::Message::encoded_len(&result_record) + 16 <= 4096);
    drop(connection);
    drop(daemon);
    let _daemon = hub.start_with_cube(&cube);
    let mut connection = hub.connection().await;
    connection
        .send(hub_frame::Body::Welcome(v1::Welcome {
            max_frame_bytes: 8192,
            max_inflight_bytes: 4096,
            ..connection.welcome_body(result_record.sequence - 1)
        }))
        .await;
    assert_eq!(connection.next().await, spoke_frame::Body::Record(result_record));
}

#[tokio::test]
async fn daemon_reports_pending_then_missing_driver_preflights() {
    let mut hub = Fixture::new().await;
    std::fs::create_dir_all(hub.root().join("credentials")).unwrap();
    for dir in [hub.root(), hub.root().join("credentials")] {
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    }
    std::fs::write(
        hub.root().join("credentials/preflight-repos"),
        "/nonexistent/boss-preflight-repository",
    )
    .unwrap();
    std::fs::write(hub.root().join("credentials/preflight-run-pending"), "pending").unwrap();
    let _daemon = hub.start_without_tools();
    let mut connection = hub.connection().await;
    connection.welcome(0).await;
    let mut pending = false;
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if let spoke_frame::Body::Record(record) = connection.next().await {
                connection.receipt(record.sequence).await;
                if let Some(record::Body::Capacity(capacity)) = record.body {
                    let failures = capacity.inventory.unwrap().failed_preflights;
                    assert!(!failures.iter().any(|name| name == "private_homes"));
                    if failures.iter().any(|name| name == "preflight_pending") {
                        pending = true;
                    } else {
                        assert!(pending, "initial capacity must report pending readiness");
                        for name in [
                            "tool_claude",
                            "claude_credentials",
                            "tool_codex",
                            "codex_credentials",
                            "repo_access",
                            "preflight_run",
                        ] {
                            assert!(failures.iter().any(|failure| failure == name), "{failures:?}");
                        }
                        break;
                    }
                }
            }
        }
    })
    .await
    .expect("completed preflights must reach the hub");
}
