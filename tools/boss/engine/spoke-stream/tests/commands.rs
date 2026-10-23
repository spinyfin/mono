use super::support::Peer;
use super::{command, record, support::*};
use boss_protocol_spoke::v1::*;
use boss_spoke_stream::CommandOutcome;
use std::time::Duration;

#[tokio::test]
async fn spawn_cannot_require_an_unnegotiated_feature() {
    let fx = Fixture::new().await;
    let mut peer = fx.connect(hello()).await.unwrap();
    peer.welcome().await;
    let mut spawn = command("spawn");
    spawn.body = Some(command::Body::Spawn(Spawn {
        lease_id: "lease".into(),
        workspace_id: "workspace".into(),
        driver_id: "driver".into(),
        prompt_utf8: b"prompt".to_vec(),
        required_features: vec![Feature {
            name: "execution".into(),
            revision: 2,
        }],
        ..Default::default()
    }));
    assert_eq!(
        fx.manager.command("host", spawn, STEP).await,
        Err(CommandOutcome::Invalid)
    );
    assert!(fx.store.outbox.lock().unwrap().commands.is_empty());
}

#[tokio::test]
async fn revocation_closes_response_while_command_storage_is_busy() {
    let fx = Fixture::new().await;
    let mut peer = fx.connect(hello()).await.unwrap();
    peer.welcome().await;
    let pause = std::sync::Arc::new(super::store::Pause::default());
    *fx.store.pauses.append.lock().unwrap() = Some(pause.clone());
    let manager = fx.manager.clone();
    let waiting = tokio::spawn(async move { manager.command("host", command("stop"), STEP).await });
    tokio::time::timeout(STEP, pause.entered.notified()).await.unwrap();
    *fx.auth.revoked.lock().unwrap() = true;
    fx.auth
        .events
        .send(boss_host_enrollment::CredentialEvent::Revoked { host_id: "host".into() })
        .unwrap();
    assert!(peer.next().await.is_none());
    pause.release.notify_one();
    assert_eq!(waiting.await.unwrap(), Err(CommandOutcome::OutcomeUncertain));
}

#[tokio::test]
async fn receipt_is_not_success_timeout_is_uncertain_and_late_result_is_queryable() {
    let fx = Fixture::new().await;
    let mut peer = fx.connect(hello()).await.unwrap();
    peer.welcome().await;
    let manager = fx.manager.clone();
    let waiting = tokio::spawn(async move {
        manager
            .command("host", command("stop"), Duration::from_millis(200))
            .await
    });
    let sent = match peer.next().await {
        Some(hub_frame::Body::Command(c)) => c,
        other => panic!("{other:?}"),
    };
    peer.send(spoke_frame::Body::CommandReceipt(Cursor {
        journal_id: "commands".into(),
        through: sent.sequence,
    }))
    .await;
    assert_eq!(waiting.await.unwrap(), Err(CommandOutcome::OutcomeUncertain));
    assert_eq!(fx.store.outbox.lock().unwrap().commands.len(), 1);
    let result = CommandResult {
        command_id: "stop".into(),
        code: ResultCode::Ok.into(),
        value: Some(command_result::Value::Completed(Empty {})),
        ..Default::default()
    };
    let mut completed = record(1);
    completed.body = Some(record::Body::Result(result.clone()));
    peer.send(spoke_frame::Body::Record(completed)).await;
    assert!(matches!(peer.next().await, Some(hub_frame::Body::RecordReceipt(c)) if c.through == 1));
    assert_eq!(fx.manager.command("host", command("stop"), STEP).await.unwrap(), result);
    assert_eq!(fx.store.outbox.lock().unwrap().commands.len(), 1);
}

#[tokio::test]
async fn reconnect_replays_same_command_and_fences_old_waiter() {
    let fx = Fixture::new().await;
    let mut old = fx.connect(hello()).await.unwrap();
    old.welcome().await;
    let manager = fx.manager.clone();
    let waiting = tokio::spawn(async move { manager.command("host", command("stop"), STEP).await });
    let first = match old.next().await {
        Some(hub_frame::Body::Command(c)) => c,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        fx.manager.command("host", command("other"), STEP).await,
        Err(CommandOutcome::Busy)
    );
    let mut new = fx.connect(hello()).await.unwrap();
    new.welcome().await;
    assert_eq!(waiting.await.unwrap(), Err(CommandOutcome::OutcomeUncertain));
    assert!(old.next().await.is_none());
    let replay = match new.next().await {
        Some(hub_frame::Body::Command(c)) => c,
        other => panic!("{other:?}"),
    };
    assert_eq!(first, replay);
    let manager = fx.manager.clone();
    let retry = tokio::spawn(async move { manager.command("host", command("stop"), STEP).await });
    let mut result = record(1);
    result.body = Some(record::Body::Result(CommandResult {
        command_id: "stop".into(),
        code: ResultCode::OutcomeUncertain.into(),
        ..Default::default()
    }));
    new.send(spoke_frame::Body::Record(result)).await;
    assert_eq!(retry.await.unwrap().unwrap().code, ResultCode::OutcomeUncertain as i32);
    assert_eq!(fx.store.outbox.lock().unwrap().commands.len(), 1);
}

#[tokio::test]
async fn forged_result_or_receipt_cannot_complete_or_advance_a_command() {
    for result_case in [true, false] {
        let fx = Fixture::new().await;
        let mut peer = fx.connect(hello()).await.unwrap();
        peer.welcome().await;
        let manager = fx.manager.clone();
        let waiting = tokio::spawn(async move { manager.command("host", command("stop"), STEP).await });
        assert!(matches!(peer.next().await, Some(hub_frame::Body::Command(_))));
        if result_case {
            let mut forged = record(1);
            forged.run = None;
            forged.body = Some(record::Body::Result(CommandResult {
                command_id: "stop".into(),
                code: ResultCode::Ok.into(),
                value: Some(command_result::Value::Completed(Empty {})),
                ..Default::default()
            }));
            peer.send(spoke_frame::Body::Record(forged)).await;
        } else {
            peer.send(spoke_frame::Body::CommandReceipt(Cursor {
                journal_id: "commands".into(),
                through: 2,
            }))
            .await;
        }
        assert!(peer.next().await.is_none());
        assert_eq!(waiting.await.unwrap(), Err(CommandOutcome::OutcomeUncertain));
        assert!(fx.store.ingress.lock().unwrap().records.is_empty());
        assert_eq!(fx.store.outbox.lock().unwrap().received, 0);
    }
}

#[tokio::test]
async fn command_replay_preserves_unknown_fields_from_durable_bytes() {
    use boss_spoke_stream::store::CommandOutbox;
    use prost::Message;
    #[derive(Clone, PartialEq, Message)]
    struct FutureHubFrame {
        #[prost(message, optional, tag = "1")]
        welcome: Option<Welcome>,
        #[prost(bytes = "vec", tag = "3")]
        command: Vec<u8>,
    }
    let fx = Fixture::new().await;
    fx.store.append("host", command("stop")).await.unwrap();
    let expected = {
        let mut outbox = fx.store.outbox.lock().unwrap();
        let bytes = outbox.encoded.get_mut(&1).unwrap();
        bytes.extend([0xa0, 0x06, 0x07]);
        bytes.clone()
    };
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(SpokeFrame {
        body: Some(spoke_frame::Body::Hello(hello())),
    })
    .await
    .unwrap();
    let mut client = tonic::client::Grpc::new(fx.channel.clone());
    client.ready().await.unwrap();
    let mut stream: tonic::Streaming<FutureHubFrame> = client
        .streaming(
            tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(rx)),
            tonic::codegen::http::uri::PathAndQuery::from_static("/boss.spoke.v1.SpokeHub/Connect"),
            tonic::codec::ProstCodec::default(),
        )
        .await
        .unwrap()
        .into_inner();
    assert!(
        tokio::time::timeout(STEP, stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .welcome
            .is_some()
    );
    let replay = tokio::time::timeout(STEP, stream.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(replay.command, expected);
}

fn capture_command() -> Command {
    let mut capture = command("capture");
    capture.body = Some(command::Body::Capture(Capture {
        subscription_id: "sub".into(),
        interval_ms: 1000,
        max_bytes: 1024,
        ttl_seconds: 60,
    }));
    capture
}

async fn expect_command(peer: &mut Peer, id: &str, sequence: u64) {
    match peer.next().await {
        Some(hub_frame::Body::Command(c)) => assert_eq!((c.command_id.as_str(), c.sequence), (id, sequence)),
        other => panic!("{other:?}"),
    }
}

async fn receipt(peer: &Peer, through: u64) {
    peer.send(spoke_frame::Body::CommandReceipt(Cursor {
        journal_id: "commands".into(),
        through,
    }))
    .await;
}

#[tokio::test]
async fn unreplayable_command_is_durably_rejected_and_the_wire_sequence_stays_contiguous() {
    use boss_protocol_spoke::negotiate::feature;
    use boss_spoke_stream::store::CommandOutbox;
    let fx = Fixture::new().await;
    let capture = capture_command();
    fx.store.append("host", capture.clone()).await.unwrap();
    fx.store.append("host", command("stop")).await.unwrap();
    fx.store.append("host", command("later")).await.unwrap();
    // The reconnecting spoke no longer offers pane capture.
    let mut reconnect = hello();
    reconnect.features.retain(|f| f.name != feature::PANE_CAPTURE);
    let mut peer = fx.connect(reconnect.clone()).await.unwrap();
    peer.welcome().await;
    // The stale command is skipped without a hole: the spoke sees 1, 2 and acks
    // only its highest contiguous received sequence.
    expect_command(&mut peer, "stop", 1).await;
    receipt(&peer, 1).await;
    expect_command(&mut peer, "later", 2).await;
    receipt(&peer, 2).await;
    // The stream is still admitting records.
    peer.send(spoke_frame::Body::Record(record(1))).await;
    assert!(matches!(peer.next().await, Some(hub_frame::Body::RecordReceipt(c)) if c.through == 1));
    let rejected = fx.manager.command("host", capture.clone(), STEP).await.unwrap();
    assert_eq!(rejected.code, ResultCode::Rejected as i32);
    assert_eq!(rejected.command_id, "capture");
    assert_eq!(fx.store.completed("host", "capture").await.unwrap(), Some(rejected));
    // A changed request under the settled ID is not answered with the old result.
    let mut changed = capture.clone();
    changed.run.as_mut().unwrap().run_id = "other".into();
    assert_eq!(
        fx.manager.command("host", changed, STEP).await,
        Err(CommandOutcome::Invalid)
    );
    let mut changed = command("capture");
    changed.not_after_unix_ms += 1;
    assert_eq!(
        fx.manager.command("host", changed, STEP).await,
        Err(CommandOutcome::Invalid)
    );
    // Across a reconnect the receipt is still a contiguous prefix and nothing replays.
    drop(peer);
    reconnect.last_record = 1;
    reconnect.received_commands = Some(Cursor {
        journal_id: "commands".into(),
        through: 2,
    });
    let mut again = fx.connect(reconnect).await.unwrap();
    again.welcome().await;
    let manager = fx.manager.clone();
    let waiting = tokio::spawn(async move { manager.command("host", command("final"), STEP).await });
    expect_command(&mut again, "final", 3).await;
    receipt(&again, 3).await;
    waiting.abort();
}

#[tokio::test]
async fn undecodable_or_invalid_stored_commands_settle_under_their_real_id() {
    use boss_spoke_stream::store::CommandOutbox;
    use prost::Message;
    for undecodable in [false, true] {
        let fx = Fixture::new().await;
        fx.store.append("host", command("broken")).await.unwrap();
        fx.store.append("host", command("stop")).await.unwrap();
        {
            let mut outbox = fx.store.outbox.lock().unwrap();
            let bytes = if undecodable {
                vec![0xff, 0xff, 0xff]
            } else {
                // An older hub reading a body it does not know: decodes, fails validation.
                let mut c = outbox.commands[0].clone();
                c.body = None;
                c.encode_to_vec()
            };
            outbox.encoded.insert(1, bytes);
        }
        let mut peer = fx.connect(hello()).await.unwrap();
        peer.welcome().await;
        expect_command(&mut peer, "stop", 1).await;
        let rejected = fx.store.completed("host", "broken").await.unwrap().unwrap();
        assert_eq!(rejected.code, ResultCode::Rejected as i32);
        assert_eq!(rejected.command_id, "broken");
        assert_eq!(
            fx.manager.command("host", command("broken"), STEP).await.unwrap(),
            rejected
        );
    }
}

#[tokio::test]
async fn rejecting_notifies_a_matching_waiter_with_the_real_id() {
    let fx = Fixture::new().await;
    let mut peer = fx.connect(hello()).await.unwrap();
    peer.welcome().await;
    fx.store
        .strip_appended_body
        .store(true, std::sync::atomic::Ordering::Release);
    let result = fx.manager.command("host", command("unknown-body"), STEP).await.unwrap();
    assert_eq!(
        (result.command_id.as_str(), result.code),
        ("unknown-body", ResultCode::Rejected as i32)
    );
    fx.store
        .strip_appended_body
        .store(false, std::sync::atomic::Ordering::Release);
    // The freed slot is reused by the next command.
    let manager = fx.manager.clone();
    let waiting = tokio::spawn(async move { manager.command("host", command("stop"), STEP).await });
    expect_command(&mut peer, "stop", 1).await;
    waiting.abort();
}

#[tokio::test]
async fn outbox_ordering_violations_fail_the_stream_and_settle_nothing() {
    use boss_spoke_stream::store::CommandOutbox;
    let fx = Fixture::new().await;
    fx.store.append("host", command("stop")).await.unwrap();
    {
        // A duplicated/out-of-order embedded sequence: the outbox broke its contract.
        use prost::Message;
        let mut outbox = fx.store.outbox.lock().unwrap();
        let mut c = outbox.commands[0].clone();
        c.sequence = 7;
        outbox.encoded.insert(1, c.encode_to_vec());
    }
    let mut peer = fx.connect(hello()).await.unwrap();
    peer.welcome().await;
    let status = peer.closed().await.expect("stream fails");
    assert_eq!(status.code(), tonic::Code::Internal);
    assert_eq!(fx.store.completed("host", "stop").await.unwrap(), None);
}
