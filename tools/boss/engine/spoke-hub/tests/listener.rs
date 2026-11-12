use std::net::SocketAddr;
use std::sync::Arc;

use boss_host_enrollment::{HostCredentials, pem_encode};
use boss_protocol_spoke::v1::enrollment_client::EnrollmentClient;
use boss_protocol_spoke::v1::spoke_hub_client::SpokeHubClient;
use boss_protocol_spoke::v1::{self, CommandResult, EnrollRequest, ResultCode, record, spoke_frame};
use boss_spoke_hub::HubStream;
use boss_spoke_hub::projection::{
    Applied, Decision, EventObservation, ObservationApplier, Origin, PROPOSAL_DECISION, PaneObservation,
    ProjectorConfig, Proposed,
};
use boss_spoke_stream::CommandOutcome;
use boss_spoke_stream::store::CommandAdmission;
use boss_work_db::WorkDb;
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Code;
use tonic::Status;
use tonic::Streaming;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Identity};

use crate::support::*;

struct Spoke {
    key: KeyPair,
}

impl Spoke {
    fn new() -> Self {
        Self {
            key: KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key"),
        }
    }

    fn csr(&self) -> Vec<u8> {
        CertificateParams::default()
            .serialize_request(&self.key)
            .expect("csr")
            .der()
            .to_vec()
    }
}

struct Running {
    addr: SocketAddr,
    pki: ServerPki,
    task: tokio::task::JoinHandle<()>,
    stream: HubStream,
}

impl Running {
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn start(db: WorkDb, credentials: Arc<HostCredentials>) -> Running {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pki = ServerPki::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let identity = pki.identity();
    let stream = hub_stream(db, credentials);
    let serving = stream.clone();
    let task = tokio::spawn(async move {
        let _ = serving.serve(listener, identity).await;
    });
    Running {
        addr,
        pki,
        task,
        stream,
    }
}

async fn dial(hub: &Running, identity: Option<Identity>) -> Result<Channel, tonic::transport::Error> {
    let mut tls = ClientTlsConfig::new()
        .ca_certificate(hub.pki.trusted())
        .domain_name("localhost");
    if let Some(identity) = identity {
        tls = tls.identity(identity);
    }
    Endpoint::from_shared(format!("https://127.0.0.1:{}", hub.addr.port()))?
        .tls_config(tls)?
        .connect()
        .await
}

fn identity_of(spoke: &Spoke, cert: &v1::Certificate) -> Identity {
    Identity::from_pem(pem_encode("CERTIFICATE", &cert.leaf_der), spoke.key.serialize_pem())
}

struct Peer {
    tx: mpsc::Sender<v1::SpokeFrame>,
    rx: Streaming<v1::HubFrame>,
}

impl Peer {
    async fn next(&mut self) -> Option<v1::hub_frame::Body> {
        loop {
            let message = tokio::time::timeout(STEP, self.rx.message())
                .await
                .expect("bounded stream read");
            let Ok(Some(message)) = message else {
                return None;
            };
            if let Some(v1::hub_frame::Body::Pulse(pulse)) = message.body {
                let _ = self
                    .tx
                    .send(v1::SpokeFrame {
                        body: Some(spoke_frame::Body::PulseReply(pulse)),
                    })
                    .await;
            } else {
                return message.body;
            }
        }
    }

    async fn closed(&mut self) -> Option<tonic::Status> {
        loop {
            let message = tokio::time::timeout(STEP, self.rx.message())
                .await
                .expect("bounded stream read");
            match message {
                Ok(Some(frame)) => {
                    if let Some(v1::hub_frame::Body::Pulse(pulse)) = frame.body {
                        let _ = self
                            .tx
                            .send(v1::SpokeFrame {
                                body: Some(spoke_frame::Body::PulseReply(pulse)),
                            })
                            .await;
                    }
                }
                Ok(None) => return None,
                Err(status) => return Some(status),
            }
        }
    }
}

async fn connect(channel: Channel, hello: v1::Hello) -> Result<Peer, Box<Status>> {
    let (tx, rx) = mpsc::channel(8);
    tx.send(v1::SpokeFrame {
        body: Some(spoke_frame::Body::Hello(hello)),
    })
    .await
    .unwrap();
    let rx = tokio::time::timeout(STEP, SpokeHubClient::new(channel).connect(ReceiverStream::new(rx)))
        .await
        .unwrap()?
        .into_inner();
    Ok(Peer { tx, rx })
}

async fn enroll_edge(hub: &Running, credentials: &HostCredentials) -> (Spoke, v1::Certificate) {
    credentials.authorize_host("edge-1", "test").expect("authorize");
    let grant = credentials.issue_enrollment_token("edge-1", "job-1").expect("token");
    let spoke = Spoke::new();
    let mut anonymous = EnrollmentClient::new(dial(hub, None).await.expect("dial"));
    let cert = anonymous
        .enroll(EnrollRequest {
            host_id: "edge-1".into(),
            job_id: "job-1".into(),
            request_id: "req-1".into(),
            token: grant.token.as_bytes().to_vec(),
            csr_der: spoke.csr(),
        })
        .await
        .expect("enroll")
        .into_inner();
    (spoke, cert)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spoke_enrolls_then_connects_and_commits_a_record() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let (spoke, cert) = enroll_edge(&hub, &fx.credentials).await;
    assert_eq!(cert.host_id, "edge-1");

    let unauthenticated = connect(dial(&hub, None).await.expect("anonymous"), hello("edge-1")).await;
    assert_eq!(
        match unauthenticated {
            Err(status) => status.code(),
            Ok(_) => panic!("anonymous connect must fail"),
        },
        Code::Unauthenticated
    );

    let channel = dial(&hub, Some(identity_of(&spoke, &cert)))
        .await
        .expect("authenticated dial");
    let mut peer = connect(channel, hello("edge-1")).await.expect("connect");
    let welcome = match peer.next().await {
        Some(v1::hub_frame::Body::Welcome(welcome)) => welcome,
        other => panic!("expected welcome, got {other:?}"),
    };
    assert_eq!(welcome.hub_id, "hub");
    assert_eq!(welcome.connection_generation, 1);
    assert_eq!(welcome.received_records.unwrap().through, 0);

    peer.tx
        .send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(capacity(1))),
        })
        .await
        .unwrap();
    match peer.next().await {
        Some(v1::hub_frame::Body::RecordReceipt(cursor)) => assert_eq!(cursor.through, 1),
        other => panic!("expected receipt, got {other:?}"),
    }
    assert_eq!(db.spoke_journal("edge-1").unwrap().unwrap().received, 1);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_replays_receipt_and_fences_the_old_generation() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db, fx.credentials.clone()).await;
    let (spoke, cert) = enroll_edge(&hub, &fx.credentials).await;
    let identity = identity_of(&spoke, &cert);

    let mut first = connect(dial(&hub, Some(identity.clone())).await.expect("dial"), hello("edge-1"))
        .await
        .expect("connect");
    first.next().await;
    first
        .tx
        .send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(capacity(1))),
        })
        .await
        .unwrap();
    match first.next().await {
        Some(v1::hub_frame::Body::RecordReceipt(cursor)) => assert_eq!(cursor.through, 1),
        other => panic!("expected receipt, got {other:?}"),
    }

    let mut reconnect = hello("edge-1");
    reconnect.last_record = 1;
    let mut second = connect(dial(&hub, Some(identity)).await.expect("dial"), reconnect)
        .await
        .expect("reconnect");
    let welcome = match second.next().await {
        Some(v1::hub_frame::Body::Welcome(welcome)) => welcome,
        other => panic!("expected welcome, got {other:?}"),
    };
    assert!(welcome.connection_generation > 1);
    assert_eq!(welcome.received_records.unwrap().through, 1);

    first
        .tx
        .send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(capacity(2))),
        })
        .await
        .ok();
    assert_eq!(
        first.closed().await.expect("fenced stream ends with an error").code(),
        Code::Aborted
    );

    second
        .tx
        .send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(capacity(1))),
        })
        .await
        .unwrap();
    match second.next().await {
        Some(v1::hub_frame::Body::RecordReceipt(cursor)) => assert_eq!(cursor.through, 1),
        other => panic!("expected duplicate receipt, got {other:?}"),
    }

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn first_contact_with_a_populated_journal_replays_from_the_start() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let (spoke, cert) = enroll_edge(&hub, &fx.credentials).await;

    // The spoke produced records before it ever reached this hub.
    let mut populated = hello("edge-1");
    populated.last_record = 3;
    populated.first_retained = 1;
    let mut peer = connect(
        dial(&hub, Some(identity_of(&spoke, &cert))).await.expect("dial"),
        populated,
    )
    .await
    .expect("connect");
    let welcome = match peer.next().await {
        Some(v1::hub_frame::Body::Welcome(welcome)) => welcome,
        other => panic!("expected welcome, got {other:?}"),
    };
    assert_eq!(welcome.received_records.unwrap().through, 0);
    for sequence in 1..=3 {
        peer.tx
            .send(v1::SpokeFrame {
                body: Some(spoke_frame::Body::Record(capacity(sequence))),
            })
            .await
            .unwrap();
        match peer.next().await {
            Some(v1::hub_frame::Body::RecordReceipt(cursor)) => assert_eq!(cursor.through, sequence),
            other => panic!("expected receipt, got {other:?}"),
        }
    }
    assert_eq!(db.spoke_journal("edge-1").unwrap().unwrap().received, 3);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_result_at_a_pruned_sequence_does_not_resolve_the_waiter_or_fabricate_a_receipt() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    spoke_ready(&db, "edge-1");
    let owned = db.reserve_spoke_assignment("exec_1", "edge-1").unwrap();
    let run = proto_run(&run_key(&owned));
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let (spoke, cert) = enroll_edge(&hub, &fx.credentials).await;
    let mut peer = connect(
        dial(&hub, Some(identity_of(&spoke, &cert))).await.expect("dial"),
        hello("edge-1"),
    )
    .await
    .expect("connect");
    match peer.next().await {
        Some(v1::hub_frame::Body::Welcome(_)) => {}
        other => panic!("expected welcome, got {other:?}"),
    }

    peer.tx
        .send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(capacity(1))),
        })
        .await
        .unwrap();
    match peer.next().await {
        Some(v1::hub_frame::Body::RecordReceipt(cursor)) => assert_eq!(cursor.through, 1),
        other => panic!("expected receipt, got {other:?}"),
    }
    assert_eq!(db.prune_spoke_ingress("edge-1", 1).unwrap(), 1);
    assert!(db.spoke_ingress_frame("edge-1", 1).unwrap().is_none());

    let manager = hub.stream.manager().clone();
    let waiting = tokio::spawn({
        let command = stop_command("c1", run.clone());
        async move { manager.command("edge-1", CommandAdmission::Start, command, STEP).await }
    });
    match peer.next().await {
        Some(v1::hub_frame::Body::Command(command)) => assert_eq!(command.command_id, "c1"),
        other => panic!("expected command, got {other:?}"),
    }

    let result = CommandResult {
        command_id: "c1".into(),
        code: ResultCode::Ok.into(),
        ..Default::default()
    };
    peer.tx
        .send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(v1::Record {
                sequence: 1,
                observed: Some(stamp()),
                run: Some(run),
                body: Some(record::Body::Result(result)),
            })),
        })
        .await
        .unwrap();
    match peer.next().await {
        Some(v1::hub_frame::Body::RecordReceipt(_)) => {
            panic!("pruned fabricated result must not mint a receipt")
        }
        Some(other) => panic!("unexpected hub frame after pruned result, got {other:?}"),
        None => {}
    }
    let outcome = waiting.await.expect("command task");
    assert_eq!(
        outcome,
        Err(CommandOutcome::OutcomeUncertain),
        "waiter must not resolve with success"
    );
    assert!(db.host_command_result("edge-1", "c1").unwrap().is_none());
    assert_eq!(db.spoke_journal("edge-1").unwrap().unwrap().received, 1);

    hub.stop().await;
}

/// Accepts every proposal and counts how many it was asked to apply.
#[derive(Default)]
struct Accepting(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl ObservationApplier for Accepting {
    async fn event(&self, _: &EventObservation) -> Applied {
        Applied::Done
    }

    async fn proposal(&self, _: &Origin, _: boss_protocol::ProposalKind, _: &serde_json::Value, _: &str) -> Proposed {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Proposed::Decided(Decision {
            accepted: true,
            reason: String::new(),
            result: serde_json::json!({"state": "applied"}),
        })
    }

    async fn pane(&self, _: &PaneObservation) -> Applied {
        Applied::Done
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_proposal_gets_its_decision_over_the_live_stream_and_is_settled_once() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    spoke_ready(&db, "edge-1");
    let owned = db.reserve_spoke_assignment("exec_1", "edge-1").unwrap();
    let run = proto_run(&run_key(&owned));
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let (spoke, cert) = enroll_edge(&hub, &fx.credentials).await;
    let mut peer = connect(
        dial(&hub, Some(identity_of(&spoke, &cert))).await.expect("dial"),
        hello("edge-1"),
    )
    .await
    .expect("connect");
    let welcome = match peer.next().await {
        Some(v1::hub_frame::Body::Welcome(welcome)) => welcome,
        other => panic!("expected welcome, got {other:?}"),
    };

    let applier = Arc::new(Accepting::default());
    let config = ProjectorConfig::builder()
        .sweep_interval(std::time::Duration::from_millis(50))
        .delivery_interval(std::time::Duration::from_millis(100))
        .build();
    let projecting = tokio::spawn(hub.stream.projector(db.clone(), applier.clone(), config).run());

    // The worker's `done`, queued on the spoke while the hub may have been away.
    let proposal = v1::Record {
        sequence: 1,
        observed: Some(stamp()),
        run: Some(run.clone()),
        body: Some(record::Body::Proposal(v1::Proposal {
            proposal_id: "key-1".into(),
            kind: v1::ProposalKind::RunDone.into(),
            payload_contract: "proposal-json/1".into(),
            canonical_json: serde_json::to_vec(&serde_json::json!({
                "kind": boss_protocol::ProposalKind::RunDone,
                "payload": {"outcome": "delivered", "summary": "queued"},
                "idempotency_key": "key-1",
            }))
            .unwrap(),
            ..Default::default()
        })),
    };
    peer.tx
        .send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(proposal)),
        })
        .await
        .unwrap();
    match peer.next().await {
        Some(v1::hub_frame::Body::RecordReceipt(cursor)) => assert_eq!(cursor.through, 1),
        other => panic!("expected receipt, got {other:?}"),
    }

    // The hub applies it and answers over the same stream, as a command.
    let command = match peer.next().await {
        Some(v1::hub_frame::Body::Command(command)) => command,
        other => panic!("expected the decision command, got {other:?}"),
    };
    assert_eq!(command.run.as_ref(), Some(&run));
    let Some(v1::command::Body::ProposalDecision(decision)) = &command.body else {
        panic!("expected a proposal decision, got {:?}", command.body)
    };
    assert_eq!(
        (decision.proposal_id.as_str(), decision.code),
        ("key-1", ResultCode::Ok as i32)
    );
    assert_eq!(
        db.spoke_effect("edge-1", PROPOSAL_DECISION, &format!("{}:key-1", run.run_id))
            .unwrap()
            .unwrap()
            .state,
        boss_work_db::SpokeEffectState::Pending,
        "owed until the spoke answers"
    );

    // The spoke acknowledges the command and reports it applied.
    peer.tx
        .send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::CommandReceipt(v1::Cursor {
                journal_id: welcome.command_journal_id.clone(),
                through: command.sequence,
            })),
        })
        .await
        .unwrap();
    peer.tx
        .send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(v1::Record {
                sequence: 2,
                observed: Some(stamp()),
                run: Some(run.clone()),
                body: Some(record::Body::Result(CommandResult {
                    command_id: command.command_id.clone(),
                    code: ResultCode::Ok.into(),
                    ..Default::default()
                })),
            })),
        })
        .await
        .unwrap();
    match peer.next().await {
        Some(v1::hub_frame::Body::RecordReceipt(cursor)) => assert_eq!(cursor.through, 2),
        other => panic!("expected the result's receipt, got {other:?}"),
    }
    for _ in 0..200 {
        let effect = db
            .spoke_effect("edge-1", PROPOSAL_DECISION, &format!("{}:key-1", run.run_id))
            .unwrap()
            .unwrap();
        if effect.state == boss_work_db::SpokeEffectState::Done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let effect = db
        .spoke_effect("edge-1", PROPOSAL_DECISION, &format!("{}:key-1", run.run_id))
        .unwrap()
        .unwrap();
    assert_eq!(effect.state, boss_work_db::SpokeEffectState::Done);
    // Nothing is sent or applied a second time, even after several sweeps.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(applier.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    match tokio::time::timeout(std::time::Duration::from_millis(300), peer.next()).await {
        Err(_) | Ok(None) => {}
        Ok(Some(other)) => panic!("a settled decision was sent again: {other:?}"),
    }
    projecting.abort();
    hub.stop().await;
}
