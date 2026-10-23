use super::{record, support::*};
use boss_host_enrollment::CredentialEvent;
use boss_protocol_spoke::v1::*;
use std::{
    sync::atomic::Ordering,
    time::{Duration, SystemTime},
};
use tonic::Code;

#[tokio::test]
async fn certificate_required_and_cannot_claim_another_host() {
    let fx = Fixture::new().await;
    assert_eq!(
        connect(fx.anonymous.clone(), hello()).await.err().unwrap().code(),
        Code::Unauthenticated
    );
    let mut wrong = hello();
    wrong.host_id = "other".into();
    assert_eq!(fx.connect(wrong).await.err().unwrap().code(), Code::PermissionDenied);
    assert_eq!(fx.store.ingress.lock().unwrap().generation, 0);
}

#[tokio::test]
async fn negotiation_rejects_required_features_but_allows_different_builds() {
    let fx = Fixture::new().await;
    let mut incompatible = hello();
    incompatible.min_epoch = 2;
    incompatible.max_epoch = 2;
    let mut peer = fx.connect(incompatible).await.unwrap();
    assert!(matches!(peer.next().await, Some(hub_frame::Body::Reject(r)) if r.code == "no_common_epoch"));
    let mut missing = hello();
    missing.features.retain(|f| f.name != "journal");
    let mut peer = fx.connect(missing).await.unwrap();
    assert!(matches!(peer.next().await, Some(hub_frame::Body::Reject(r)) if r.code == "missing_required_feature"));
    let mut compatible = hello();
    compatible.build = "another-build".into();
    compatible.features.retain(|f| f.name != "pane-capture");
    let mut peer = fx.connect(compatible).await.unwrap();
    assert!(!peer.welcome().await.features.iter().any(|f| f.name == "pane-capture"));
    let mut capture = record(1);
    capture.body = Some(record::Body::Capture(PaneCapture {
        subscription_id: "view".into(),
        text_utf8: vec![],
        truncated: false,
    }));
    peer.send(spoke_frame::Body::Record(capture)).await;
    assert!(peer.next().await.is_none());
    assert!(fx.store.ingress.lock().unwrap().records.is_empty());
}

#[tokio::test]
async fn reconnect_fences_old_reader_and_writer_and_returns_durable_receipt() {
    let fx = Fixture::new().await;
    let mut old = fx.connect(hello()).await.unwrap();
    let first = old.welcome().await;
    old.send(spoke_frame::Body::Record(record(1))).await;
    assert!(matches!(old.next().await, Some(hub_frame::Body::RecordReceipt(c)) if c.through == 1));
    let mut reconnect = hello();
    reconnect.last_record = 1;
    let mut new = fx.connect(reconnect).await.unwrap();
    let second = new.welcome().await;
    assert!(second.connection_generation > first.connection_generation);
    assert_eq!(second.received_records.unwrap().through, 1);
    // Even if gRPC buffered this old frame, the new generation fences admission.
    let _ = old
        .tx
        .send(SpokeFrame {
            body: Some(spoke_frame::Body::Record(record(2))),
        })
        .await;
    // A deliberate fence is an error status, never a clean OK shutdown.
    assert_eq!(
        old.closed().await.expect("fenced stream ends with an error").code(),
        Code::Aborted
    );
    new.send(spoke_frame::Body::Record(record(1))).await;
    assert!(matches!(new.next().await, Some(hub_frame::Body::RecordReceipt(c)) if c.through == 1));
    assert_eq!(fx.store.ingress.lock().unwrap().records.len(), 1);
}

#[tokio::test]
async fn live_other_instance_is_challenged_and_refused() {
    let fx = Fixture::new().await;
    let mut old = fx.connect(hello()).await.unwrap();
    old.welcome().await;
    let mut other = hello();
    other.instance_nonce = vec![2; 16];
    tokio::select! {
        response = fx.connect(other) => assert_eq!(response.err().unwrap().code(), Code::AlreadyExists),
        unexpected = old.next() => panic!("old stream should only receive challenges: {unexpected:?}"),
    }
    old.send(spoke_frame::Body::Record(record(1))).await;
    assert!(matches!(old.next().await, Some(hub_frame::Body::RecordReceipt(_))));
}

#[tokio::test]
async fn unanswered_challenge_allows_replacement() {
    let fx = Fixture::new().await;
    let mut old = fx.connect(hello()).await.unwrap();
    old.welcome().await;
    let mut other = hello();
    other.instance_nonce = vec![2; 16];
    let mut new = fx.connect(other).await.unwrap();
    assert!(new.welcome().await.connection_generation > 1);
    assert!(old.next().await.is_none());
}

#[tokio::test]
async fn replay_gaps_and_journal_changes_do_not_displace_a_valid_stream() {
    let fx = Fixture::new().await;
    let mut peer = fx.connect(hello()).await.unwrap();
    peer.welcome().await;
    for bad in [
        Hello {
            first_retained: 2,
            last_record: 2,
            ..hello()
        },
        Hello {
            journal_id: "lost".into(),
            ..hello()
        },
    ] {
        assert_eq!(fx.connect(bad).await.err().unwrap().code(), Code::FailedPrecondition);
    }
    peer.send(spoke_frame::Body::Record(record(1))).await;
    assert!(matches!(peer.next().await, Some(hub_frame::Body::RecordReceipt(c)) if c.through == 1));
}

#[tokio::test]
async fn failed_commit_cross_run_gap_and_changed_duplicate_never_get_receipts() {
    for case in 0..4 {
        let fx = Fixture::new().await;
        let mut peer = fx.connect(hello()).await.unwrap();
        peer.welcome().await;
        let mut incoming = record(1);
        match case {
            0 => fx.store.fail_commit.store(true, Ordering::Release),
            1 => incoming.run.as_mut().unwrap().spawn_token = "wrong-owner".into(),
            2 => incoming.sequence = 2,
            _ => {
                peer.send(spoke_frame::Body::Record(incoming.clone())).await;
                assert!(matches!(peer.next().await, Some(hub_frame::Body::RecordReceipt(_))));
                incoming.observed.as_mut().unwrap().awake_ns += 1;
            }
        }
        peer.send(spoke_frame::Body::Record(incoming)).await;
        assert!(peer.next().await.is_none());
        assert_eq!(fx.store.ingress.lock().unwrap().records.len(), usize::from(case == 3));
    }
}

#[tokio::test]
async fn revocation_and_shortened_expiry_close_existing_streams() {
    for revoke in [true, false] {
        let fx = Fixture::new().await;
        let mut peer = fx.connect(hello()).await.unwrap();
        peer.welcome().await;
        let event = if revoke {
            *fx.auth.revoked.lock().unwrap() = true;
            CredentialEvent::Revoked { host_id: "host".into() }
        } else {
            let until = SystemTime::now() + Duration::from_millis(50);
            fx.auth.identity.lock().unwrap().valid_until = until;
            CredentialEvent::Superseded {
                host_id: "host".into(),
                serial: "serial".into(),
                valid_until: until,
            }
        };
        fx.auth.events.send(event).unwrap();
        assert!(peer.next().await.is_none());
        assert_eq!(fx.connect(hello()).await.err().unwrap().code(), Code::Unauthenticated);
    }
}

#[tokio::test]
async fn hello_deadline_is_bounded() {
    let fx = Fixture::new().await;
    let (_tx, rx) = tokio::sync::mpsc::channel::<SpokeFrame>(1);
    let error = tokio::time::timeout(
        STEP,
        spoke_hub_client::SpokeHubClient::new(fx.channel.clone())
            .connect(tokio_stream::wrappers::ReceiverStream::new(rx)),
    )
    .await
    .unwrap()
    .err()
    .unwrap();
    assert_eq!(error.code(), Code::DeadlineExceeded);
}

#[tokio::test]
async fn certificate_expiry_and_lost_credential_events_fail_closed() {
    for lagged in [false, true] {
        let fx = Fixture::new().await;
        if !lagged {
            fx.auth.identity.lock().unwrap().valid_until = SystemTime::now() + Duration::from_millis(200);
        }
        let mut peer = fx.connect(hello()).await.unwrap();
        peer.welcome().await;
        if lagged {
            *fx.auth.revoked.lock().unwrap() = true;
            // No yield: force this subscriber to miss the revocation, then
            // observe only unrelated events. Lagged must revalidate from storage.
            fx.auth
                .events
                .send(CredentialEvent::Revoked { host_id: "host".into() })
                .unwrap();
            for _ in 0..8 {
                fx.auth
                    .events
                    .send(CredentialEvent::Revoked {
                        host_id: "other".into(),
                    })
                    .unwrap();
            }
        }
        assert!(peer.next().await.is_none());
    }
}

#[tokio::test]
async fn oversized_frames_are_rejected_before_admission() {
    let fx = Fixture::new().await;
    let oversized = Hello {
        build: "x".repeat(boss_protocol_spoke::limits::MAX_FRAME_BYTES as usize),
        ..hello()
    };
    assert!(fx.connect(oversized).await.is_err());
    assert_eq!(fx.store.ingress.lock().unwrap().generation, 0);
}

#[tokio::test]
async fn original_unknown_fields_survive_to_the_receipt_sink() {
    use prost::Message;
    // A newer schema's frame, sent through tonic to the same service route.
    #[derive(Clone, PartialEq, Message)]
    struct FutureFrame {
        #[prost(message, optional, tag = "1")]
        hello: Option<Hello>,
        #[prost(bytes = "vec", tag = "2")]
        record: Vec<u8>,
    }
    let fx = Fixture::new().await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tx.send(FutureFrame {
        hello: Some(hello()),
        record: vec![],
    })
    .await
    .unwrap();
    let mut client = tonic::client::Grpc::new(fx.channel.clone());
    client.ready().await.unwrap();
    let mut stream: tonic::Streaming<HubFrame> = client
        .streaming(
            tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(rx)),
            tonic::codegen::http::uri::PathAndQuery::from_static("/boss.spoke.v1.SpokeHub/Connect"),
            tonic::codec::ProstCodec::default(),
        )
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        stream.message().await.unwrap().unwrap().body,
        Some(hub_frame::Body::Welcome(_))
    ));
    let mut raw = record(1).encode_to_vec();
    raw.extend([0xa0, 0x06, 0x07]); // Unknown Record field 100 = 7.
    let future = FutureFrame {
        hello: None,
        record: raw,
    };
    let expected = future.encode_to_vec();
    tx.send(future).await.unwrap();
    loop {
        let message = tokio::time::timeout(STEP, stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(message.body, Some(hub_frame::Body::RecordReceipt(_))) {
            break;
        }
    }
    assert_eq!(fx.store.ingress.lock().unwrap().records[&1], expected);
}

#[tokio::test]
async fn slow_driver_does_not_let_a_contender_displace_a_live_daemon() {
    let fx = Fixture::new().await;
    let mut old = fx.connect(hello()).await.unwrap();
    old.welcome().await;
    // Settle the opening pulse so only the contender's challenge is outstanding.
    old.send(spoke_frame::Body::Record(record(1))).await;
    assert!(matches!(old.next().await, Some(hub_frame::Body::RecordReceipt(c)) if c.through == 1));
    old.settle_pulse().await;
    // Hold the driver in commit while the contender queues behind the host gate,
    // then stall its next command fetch for longer than the pulse timeout.
    let commit = std::sync::Arc::new(super::store::Pause::default());
    *fx.store.pauses.commit.lock().unwrap() = Some(commit.clone());
    old.send(spoke_frame::Body::Record(record(2))).await;
    tokio::time::timeout(STEP, commit.entered.notified()).await.unwrap();
    let next = std::sync::Arc::new(super::store::Pause::default());
    *fx.store.pauses.next.lock().unwrap() = Some(next.clone());
    let mut other = hello();
    other.instance_nonce = vec![2; 16];
    let release = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        commit.release.notify_one();
        tokio::time::timeout(STEP, next.entered.notified()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        next.release.notify_one();
        std::future::pending::<()>().await
    };
    let live_peer = async {
        loop {
            match old.next().await {
                Some(hub_frame::Body::RecordReceipt(_)) => {}
                other => panic!("unexpected frame for the live daemon: {other:?}"),
            }
        }
    };
    tokio::select! {
        response = fx.connect(other) => assert_eq!(response.err().unwrap().code(), Code::AlreadyExists),
        _ = release => unreachable!(),
        _ = live_peer => unreachable!(),
    }
}

#[tokio::test]
async fn credential_lost_during_storage_admission_never_gets_a_stream() {
    for (blocked_open, expire) in [(false, false), (true, false), (true, true)] {
        let fx = Fixture::new().await;
        if expire {
            fx.auth.identity.lock().unwrap().valid_until = SystemTime::now() + Duration::from_millis(400);
        }
        let pause = std::sync::Arc::new(super::store::Pause::default());
        if blocked_open {
            *fx.store.pauses.open.lock().unwrap() = Some(pause.clone());
        } else {
            *fx.store.pauses.resume.lock().unwrap() = Some(pause.clone());
        }
        let channel = fx.channel.clone();
        let attempt = tokio::spawn(async move { connect(channel, hello()).await });
        tokio::time::timeout(STEP, pause.entered.notified()).await.unwrap();
        if !expire {
            *fx.auth.revoked.lock().unwrap() = true;
            fx.auth
                .events
                .send(CredentialEvent::Revoked { host_id: "host".into() })
                .unwrap();
        }
        let refused = attempt.await.unwrap().err().expect("no Welcome for a dead credential");
        assert_eq!(refused.code(), Code::Unauthenticated);
        assert_eq!(fx.store.ingress.lock().unwrap().generation, 0);
        assert_eq!(fx.store.outbox.lock().unwrap().received, 0);
    }
}
