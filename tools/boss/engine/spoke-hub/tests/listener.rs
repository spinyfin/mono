use std::net::SocketAddr;
use std::sync::Arc;

use boss_host_enrollment::{HostCredentials, pem_encode};
use boss_protocol_spoke::v1::enrollment_client::EnrollmentClient;
use boss_protocol_spoke::v1::spoke_hub_client::SpokeHubClient;
use boss_protocol_spoke::v1::{self, EnrollRequest, spoke_frame};
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
    let task = tokio::spawn(async move {
        let _ = stream.serve(listener, identity).await;
    });
    Running { addr, pki, task }
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
