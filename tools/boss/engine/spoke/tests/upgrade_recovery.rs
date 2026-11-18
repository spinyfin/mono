use super::*;
use prost::Message;

#[tokio::test]
async fn interrupted_workspace_waits_for_welcome_and_is_never_rejected() {
    for management in [false, true] {
        let mut hub = Fixture::new().await;
        let daemon = hub.start();
        let connection = hub.connection().await;
        let welcome = connection.welcome_body(0);
        drop(daemon);
        drop(connection);
        let mut journal = open_after_daemon_exit(&hub.root());
        journal.welcome(&welcome).unwrap();
        let command = v1::Command {
            sequence: 1,
            command_id: "interrupted".into(),
            not_after_unix_ms: i64::MAX,
            body: Some(command::Body::Workspace(v1::WorkspaceOp {
                kind: v1::WorkspaceKind::ListRepos.into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        journal.receive_command(&command.encode_to_vec()).unwrap();
        assert!(!journal.begin_pending().unwrap().unwrap().1);
        let daemon = hub.start_after_journal(journal);
        let connection = hub.connection().await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(daemon);
        drop(connection);
        let journal = open_after_daemon_exit(&hub.root());
        assert_eq!(journal.applied_commands().unwrap(), 0, "no admission before Welcome");
        let _daemon = hub.start_after_journal(journal);
        let mut connection = hub.connection().await;
        if management {
            let features = connection
                .hello
                .features
                .iter()
                .filter(|f| {
                    f.name == boss_protocol_spoke::negotiate::feature::JOURNAL
                        || f.name == boss_protocol_spoke::negotiate::feature::UPGRADE
                })
                .cloned()
                .collect();
            connection.welcome_features(0, features).await;
        } else {
            connection.welcome(0).await;
        }
        loop {
            if let spoke_frame::Body::Record(record) = connection.next().await {
                connection.receipt(record.sequence).await;
                if let Some(record::Body::Result(result)) = record.body {
                    assert_eq!(result.command_id, "interrupted");
                    assert_ne!(result.code, v1::ResultCode::Rejected as i32);
                    if management {
                        assert_eq!(result.code, v1::ResultCode::OutcomeUncertain as i32);
                    }
                    break;
                }
            }
        }
    }
}

#[tokio::test]
async fn claimed_or_active_helpers_reject_another_offer() {
    for marker in ["update-claimed.json", "upgrade-active"] {
        let mut hub = Fixture::new().await;
        std::fs::write(hub.dir.path().join(marker), b"{}").unwrap();
        let _daemon = hub.start();
        let mut connection = hub.connection().await;
        connection.welcome_features(0, super::management_features()).await;
        connection
            .send(hub_frame::Body::Command(v1::Command {
                sequence: 1,
                command_id: "busy".into(),
                not_after_unix_ms: i64::MAX,
                body: Some(command::Body::Upgrade(v1::Upgrade {
                    release_id: "1.0.2".into(),
                    manifest_url: "https://github.com/spinyfin/mono/releases/download/x/manifest.json".into(),
                    manifest_sha256: vec![0xab; 32],
                })),
                ..Default::default()
            }))
            .await;
        loop {
            if let spoke_frame::Body::Record(record) = connection.next().await {
                connection.receipt(record.sequence).await;
                if let Some(record::Body::Result(result)) = record.body {
                    assert_eq!(result.code, v1::ResultCode::Rejected as i32);
                    assert!(result.reason.contains("upgrade_busy"), "{}", result.reason);
                    break;
                }
            }
        }
        assert!(!hub.root().join("update-request/request.json").exists());
    }
}
