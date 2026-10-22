//! The `SpokeHub.Connect` stream over real mutual TLS.
//!
//! A tonic hub and a tonic spoke talk over loopback TCP with certificates from
//! a throwaway CA, exactly the transport shape the daemon and the hub stream
//! manager will use: the spoke dials out, the hub verifies the client
//! certificate, the host named in `Hello` must match the certificate, and
//! records and commands then flow in both directions on the one stream.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use boss_protocol_spoke::Validate;
use boss_protocol_spoke::identity::{certificate_names_host, host_urn};
use boss_protocol_spoke::limits::MAX_FRAME_BYTES;
use boss_protocol_spoke::negotiate::{Contracts, reject_code};
use boss_protocol_spoke::v1::spoke_hub_client::SpokeHubClient;
use boss_protocol_spoke::v1::spoke_hub_server::{SpokeHub, SpokeHubServer};
use boss_protocol_spoke::v1::{self, hub_frame, record, spoke_frame};
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig};
use tonic::{Code, Request, Response, Status, Streaming};

const STEP: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------- PKI

/// A throwaway certificate authority: the hub's, or a rogue one.
struct Authority {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

impl Authority {
    fn new(common_name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name.push(DnType::CommonName, common_name);
        let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("ca");
        Self { issuer }
    }

    fn ca_certificate(&self) -> Certificate {
        Certificate::from_pem(self.issuer.pem())
    }

    fn leaf(&self, sans: Vec<SanType>, usage: ExtendedKeyUsagePurpose) -> Identity {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::default();
        params.subject_alt_names = sans;
        params.extended_key_usages = vec![usage];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let cert = params.signed_by(&key, &self.issuer).expect("leaf");
        Identity::from_pem(cert.pem(), key.serialize_pem())
    }

    /// The hub's server certificate, valid for 127.0.0.1 / localhost.
    fn hub_identity(&self) -> Identity {
        let dns = SanType::DnsName(Ia5String::try_from("localhost").expect("dns"));
        let ip = SanType::IpAddress("127.0.0.1".parse().expect("ip"));
        self.leaf(vec![dns, ip], ExtendedKeyUsagePurpose::ServerAuth)
    }

    /// A host's client certificate, naming exactly `host_id`.
    fn host_identity(&self, host_id: &str) -> Identity {
        let urn = host_urn(host_id).expect("valid host id");
        let uri = SanType::URI(Ia5String::try_from(urn).expect("uri"));
        self.leaf(vec![uri], ExtendedKeyUsagePurpose::ClientAuth)
    }
}

// ---------------------------------------------------------------- hub

#[derive(Default)]
struct Observed {
    records: Vec<v1::Record>,
    command_receipts: Vec<u64>,
}

struct Hub {
    contracts: Contracts,
    observed: Arc<Mutex<Observed>>,
}

fn hub_frame(generation: u64, body: hub_frame::Body) -> v1::HubFrame {
    v1::HubFrame {
        connection_generation: generation,
        body: Some(body),
    }
}

/// The `urn:boss:host:*` names in the verified client certificate.
fn client_host_uris<T>(request: &Request<T>) -> Result<Vec<String>, String> {
    let chain = request.peer_certs().ok_or("no client certificate")?;
    let leaf = chain.first().ok_or("empty client certificate chain")?;
    let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|err| format!("unparseable client certificate: {err}"))?;
    let names = match cert.subject_alternative_name() {
        Ok(Some(san)) => san
            .value
            .general_names
            .iter()
            .filter_map(|name| match name {
                x509_parser::extensions::GeneralName::URI(uri) => Some((*uri).to_owned()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(names)
}

#[tonic::async_trait]
impl SpokeHub for Hub {
    type ConnectStream = ReceiverStream<Result<v1::HubFrame, Status>>;

    async fn connect(
        &self,
        request: Request<Streaming<v1::SpokeFrame>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let certificate_hosts = client_host_uris(&request).map_err(Status::unauthenticated)?;
        let mut inbound = request.into_inner();
        let Some(spoke_frame::Body::Hello(hello)) = inbound.message().await?.and_then(|frame| frame.body) else {
            return Err(Status::invalid_argument("the first frame must be Hello"));
        };
        // The certificate authenticates; Hello only claims.
        if !certificate_names_host(&hello.host_id, certificate_hosts.iter().map(String::as_str)) {
            return Err(Status::permission_denied(
                "Hello.host_id does not match the client certificate",
            ));
        }
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(drive(
            self.contracts.clone(),
            hello,
            inbound,
            tx,
            Arc::clone(&self.observed),
        ));
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// One connection: negotiate, welcome, then receipt records in order while a
/// command goes down the same stream concurrently.
async fn drive(
    contracts: Contracts,
    hello: v1::Hello,
    mut inbound: Streaming<v1::SpokeFrame>,
    tx: mpsc::Sender<Result<v1::HubFrame, Status>>,
    observed: Arc<Mutex<Observed>>,
) {
    let selection = match contracts.select(&hello) {
        Ok(selection) => selection,
        Err(rejection) => {
            let _ = tx
                .send(Ok(hub_frame(
                    0,
                    hub_frame::Body::Reject(rejection.to_reject(&contracts)),
                )))
                .await;
            return;
        }
    };
    let journal = hello.journal_id.clone();
    let welcome = v1::Welcome {
        epoch: selection.epoch,
        features: selection.to_features(),
        hub_id: "test-hub".into(),
        authority_generation: "authority-1".into(),
        connection_generation: 1,
        received_records: Some(v1::Cursor {
            journal_id: journal.clone(),
            through: 0,
        }),
        command_journal_id: "commands-1".into(),
        max_frame_bytes: MAX_FRAME_BYTES,
        max_inflight_bytes: 0,
        build: "test".into(),
    };
    let _ = tx.send(Ok(hub_frame(1, hub_frame::Body::Welcome(welcome)))).await;

    let command = v1::Command {
        sequence: 1,
        command_id: "cmd-1".into(),
        run: Some(run()),
        not_after_unix_ms: 4_102_444_800_000,
        body: Some(v1::command::Body::Stop(v1::Stop { grace_ms: 1_000 })),
    };
    let _ = tx.send(Ok(hub_frame(1, hub_frame::Body::Command(command)))).await;

    let mut contiguous = 0;
    loop {
        match inbound.message().await {
            Ok(Some(frame)) => match frame.body {
                Some(spoke_frame::Body::Record(record)) => {
                    if let Err(err) = record.validate() {
                        let _ = tx.send(Err(Status::invalid_argument(err.to_string()))).await;
                        return;
                    }
                    if record.sequence == contiguous + 1 {
                        contiguous = record.sequence;
                    }
                    observed.lock().expect("lock").records.push(record);
                    let receipt = v1::Cursor {
                        journal_id: journal.clone(),
                        through: contiguous,
                    };
                    let _ = tx.send(Ok(hub_frame(1, hub_frame::Body::RecordReceipt(receipt)))).await;
                }
                Some(spoke_frame::Body::CommandReceipt(cursor)) => {
                    observed.lock().expect("lock").command_receipts.push(cursor.through);
                }
                Some(spoke_frame::Body::PulseReply(_)) => {}
                _ => {
                    let _ = tx.send(Err(Status::invalid_argument("unexpected frame"))).await;
                    return;
                }
            },
            Ok(None) => return,
            Err(status) => {
                let _ = tx.send(Err(status)).await;
                return;
            }
        }
    }
}

struct RunningHub {
    addr: SocketAddr,
    observed: Arc<Mutex<Observed>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningHub {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = tokio::time::timeout(STEP, self.task).await;
    }
}

/// Starts a hub whose server certificate and accepted client certificates both
/// come from `hub_ca`, requiring a client certificate on every connection.
async fn start_hub(hub_ca: &Authority) -> RunningHub {
    let tls = ServerTlsConfig::new()
        .identity(hub_ca.hub_identity())
        .client_ca_root(hub_ca.ca_certificate());
    let observed = Arc::new(Mutex::new(Observed::default()));
    let hub = Hub {
        contracts: Contracts::first_release(),
        observed: Arc::clone(&observed),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let service = SpokeHubServer::new(hub).max_decoding_message_size(MAX_FRAME_BYTES as usize);
    let server = Server::builder()
        .tls_config(tls)
        .expect("server tls")
        .add_service(service)
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
            let _ = shutdown_rx.await;
        });
    let task = tokio::spawn(async move {
        let _ = server.await;
    });
    RunningHub {
        addr,
        observed,
        shutdown: Some(shutdown_tx),
        task,
    }
}

// ---------------------------------------------------------------- spoke

fn run() -> v1::Run {
    v1::Run {
        execution_id: "exec-1".into(),
        run_id: "run-1".into(),
        assignment_generation: 1,
        spawn_token: "tok-1".into(),
    }
}

fn stamp() -> v1::Stamp {
    v1::Stamp {
        boot_id: "boot-1".into(),
        wall_unix_ms: 1_700_000_000_000,
        awake_ns: 1,
        elapsed_ns: 1,
    }
}

fn spoke_hello(host_id: &str, contracts: &Contracts) -> v1::Hello {
    v1::Hello {
        host_id: host_id.into(),
        journal_id: "journal-1".into(),
        boot_id: "boot-1".into(),
        min_epoch: contracts.min_epoch,
        max_epoch: contracts.max_epoch,
        features: contracts.features.clone(),
        build: "test".into(),
        instance_nonce: vec![3; 16],
        ..Default::default()
    }
}

fn spoke_frame(body: spoke_frame::Body) -> v1::SpokeFrame {
    v1::SpokeFrame { body: Some(body) }
}

fn record_frame(sequence: u64, body: record::Body, run: Option<v1::Run>) -> v1::SpokeFrame {
    spoke_frame(spoke_frame::Body::Record(v1::Record {
        sequence,
        observed: Some(stamp()),
        run,
        body: Some(body),
    }))
}

fn event_record(sequence: u64) -> v1::SpokeFrame {
    let event = v1::WorkerEvent {
        session_id: "session-1".into(),
        event: Some(v1::worker_event::Event::Notification(format!("event {sequence}"))),
    };
    record_frame(sequence, record::Body::Event(event), Some(run()))
}

fn client_tls(trusted_hub_ca: &Authority, identity: Option<Identity>) -> ClientTlsConfig {
    let tls = ClientTlsConfig::new()
        .ca_certificate(trusted_hub_ca.ca_certificate())
        .domain_name("localhost");
    match identity {
        Some(identity) => tls.identity(identity),
        None => tls,
    }
}

async fn dial(addr: SocketAddr, tls: ClientTlsConfig) -> Result<SpokeHubClient<Channel>, tonic::transport::Error> {
    let endpoint = Endpoint::from_shared(format!("https://127.0.0.1:{}", addr.port()))?.tls_config(tls)?;
    Ok(SpokeHubClient::new(endpoint.connect().await?))
}

/// Opens `Connect` with `hello` already queued as the first frame.
async fn open(
    client: &mut SpokeHubClient<Channel>,
    hello: v1::Hello,
) -> Result<(mpsc::Sender<v1::SpokeFrame>, Streaming<v1::HubFrame>), Box<Status>> {
    let (tx, rx) = mpsc::channel(16);
    tx.send(spoke_frame(spoke_frame::Body::Hello(hello)))
        .await
        .expect("queue hello");
    let inbound = client
        .connect(ReceiverStream::new(rx))
        .await
        .map_err(Box::new)?
        .into_inner();
    Ok((tx, inbound))
}

async fn next(inbound: &mut Streaming<v1::HubFrame>) -> Result<Option<v1::HubFrame>, Box<Status>> {
    tokio::time::timeout(STEP, inbound.message())
        .await
        .expect("timed out waiting for the hub")
        .map_err(Box::new)
}

/// Whether a spoke presenting `identity` and trusting `trusted_hub_ca` gets
/// as far as a first frame from the hub.
async fn reaches_hub(addr: SocketAddr, trusted_hub_ca: &Authority, identity: Option<Identity>) -> bool {
    let attempt = async {
        let mut client = dial(addr, client_tls(trusted_hub_ca, identity)).await.ok()?;
        let (_tx, mut inbound) = open(&mut client, spoke_hello("spoke-1", &Contracts::first_release()))
            .await
            .ok()?;
        next(&mut inbound).await.ok().flatten()
    };
    tokio::time::timeout(STEP, attempt).await.expect("timed out").is_some()
}

// ---------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn records_and_commands_flow_both_ways_over_one_mtls_stream() {
    let hub_ca = Authority::new("boss test hub CA");
    let hub = start_hub(&hub_ca).await;
    let contracts = Contracts::first_release();

    let mut client = dial(hub.addr, client_tls(&hub_ca, Some(hub_ca.host_identity("spoke-1"))))
        .await
        .expect("mTLS");
    let (tx, mut inbound) = open(&mut client, spoke_hello("spoke-1", &contracts))
        .await
        .expect("stream");

    let welcome = match next(&mut inbound).await.expect("frame").and_then(|f| f.body) {
        Some(hub_frame::Body::Welcome(welcome)) => welcome,
        other => panic!("expected Welcome, got {other:?}"),
    };
    contracts
        .confirm(&welcome)
        .expect("the hub selected a contract the spoke speaks");
    assert_eq!(welcome.received_records.map(|cursor| cursor.through), Some(0));

    // The spoke streams three journaled records while the hub's command is
    // already in flight toward it.
    let producer = tokio::spawn({
        let tx = tx.clone();
        async move {
            for sequence in 1..=3 {
                tx.send(event_record(sequence)).await.expect("send");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    });

    let (mut received_command, mut receipted_through) = (None, 0);
    while receipted_through < 3 || received_command.is_none() {
        let frame = next(&mut inbound).await.expect("frame").expect("stream still open");
        frame.validate().expect("the hub only sends valid frames");
        match frame.body {
            Some(hub_frame::Body::Command(command)) => {
                assert_eq!(
                    frame.connection_generation, 1,
                    "commands carry the stream generation outside their bytes"
                );
                tx.send(spoke_frame(spoke_frame::Body::CommandReceipt(v1::Cursor {
                    journal_id: "commands-1".into(),
                    through: command.sequence,
                })))
                .await
                .expect("send receipt");
                received_command = Some(command);
            }
            Some(hub_frame::Body::RecordReceipt(cursor)) => {
                assert_eq!(cursor.journal_id, "journal-1");
                receipted_through = receipted_through.max(cursor.through);
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }
    producer.await.expect("producer");

    // The command is answered with a journaled result, sequence 4.
    let command = received_command.expect("command");
    assert!(matches!(command.body, Some(v1::command::Body::Stop(_))));
    let result = v1::CommandResult {
        command_id: command.command_id,
        code: v1::ResultCode::Ok as i32,
        ..Default::default()
    };
    tx.send(record_frame(4, record::Body::Result(result), Some(run())))
        .await
        .expect("send result");
    while receipted_through < 4 {
        if let Some(hub_frame::Body::RecordReceipt(cursor)) =
            next(&mut inbound).await.expect("frame").and_then(|f| f.body)
        {
            receipted_through = receipted_through.max(cursor.through);
        }
    }

    drop(tx);
    assert!(
        next(&mut inbound).await.expect("clean end").is_none(),
        "the hub ends the stream when the spoke closes it"
    );
    {
        let observed = hub.observed.lock().expect("lock");
        let sequences: Vec<u64> = observed.records.iter().map(|record| record.sequence).collect();
        assert_eq!(sequences, [1, 2, 3, 4]);
        assert_eq!(observed.command_receipts, [1]);
    }
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_certificates_from_the_hub_ca_are_accepted_in_either_direction() {
    let hub_ca = Authority::new("boss test hub CA");
    let rogue_ca = Authority::new("boss rogue CA");
    let hub = start_hub(&hub_ca).await;

    assert!(
        reaches_hub(hub.addr, &hub_ca, Some(hub_ca.host_identity("spoke-1"))).await,
        "control: a certificate from the hub CA works"
    );
    assert!(
        !reaches_hub(hub.addr, &hub_ca, None).await,
        "no client certificate is refused"
    );
    assert!(
        !reaches_hub(hub.addr, &hub_ca, Some(rogue_ca.host_identity("spoke-1"))).await,
        "a client certificate from another CA is refused"
    );
    assert!(
        !reaches_hub(hub.addr, &rogue_ca, Some(hub_ca.host_identity("spoke-1"))).await,
        "a spoke that does not trust the hub's server certificate refuses to talk to it"
    );
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_cannot_claim_a_host_the_certificate_does_not_name() {
    let hub_ca = Authority::new("boss test hub CA");
    let hub = start_hub(&hub_ca).await;

    let mut client = dial(hub.addr, client_tls(&hub_ca, Some(hub_ca.host_identity("spoke-1"))))
        .await
        .expect("mTLS");
    let outcome = match open(&mut client, spoke_hello("spoke-2", &Contracts::first_release())).await {
        Ok((_tx, mut inbound)) => next(&mut inbound).await.map(|_| ()),
        Err(status) => Err(status),
    };
    assert_eq!(
        outcome.expect_err("the claim is refused").code(),
        Code::PermissionDenied
    );
    assert!(hub.observed.lock().expect("lock").records.is_empty());
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incompatible_peer_gets_a_readable_reject_not_silence() {
    let hub_ca = Authority::new("boss test hub CA");
    let hub = start_hub(&hub_ca).await;

    let mut future = Contracts::first_release();
    (future.min_epoch, future.max_epoch) = (2, 3);
    let mut client = dial(hub.addr, client_tls(&hub_ca, Some(hub_ca.host_identity("spoke-1"))))
        .await
        .expect("mTLS");
    let (_tx, mut inbound) = open(&mut client, spoke_hello("spoke-1", &future))
        .await
        .expect("stream");

    let Some(hub_frame::Body::Reject(reject)) = next(&mut inbound).await.expect("frame").and_then(|f| f.body) else {
        panic!("expected Reject");
    };
    assert_eq!(reject.code, reject_code::NO_COMMON_EPOCH);
    assert_eq!((reject.supported_min_epoch, reject.supported_max_epoch), (1, 1));
    assert!(
        next(&mut inbound).await.expect("clean end").is_none(),
        "the hub closes after rejecting"
    );
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_over_the_limit_is_refused_and_the_stream_survives_no_longer() {
    let hub_ca = Authority::new("boss test hub CA");
    let hub = start_hub(&hub_ca).await;

    let mut client = dial(hub.addr, client_tls(&hub_ca, Some(hub_ca.host_identity("spoke-1"))))
        .await
        .expect("mTLS");
    let (tx, mut inbound) = open(&mut client, spoke_hello("spoke-1", &Contracts::first_release()))
        .await
        .expect("stream");
    assert!(matches!(
        next(&mut inbound).await.expect("frame").and_then(|f| f.body),
        Some(hub_frame::Body::Welcome(_))
    ));

    let oversized = v1::TranscriptDelta {
        generation: "g".into(),
        data: vec![0; MAX_FRAME_BYTES as usize + 1],
        ..Default::default()
    };
    tx.send(record_frame(1, record::Body::Transcript(oversized), Some(run())))
        .await
        .expect("send");

    // The hub may deliver its already-queued command before it notices.
    let status = loop {
        match next(&mut inbound).await {
            Err(status) => break status,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the oversized frame must not be accepted"),
        }
    };
    assert!(
        matches!(status.code(), Code::OutOfRange | Code::ResourceExhausted),
        "{status}"
    );
    assert!(
        hub.observed.lock().expect("lock").records.is_empty(),
        "nothing was applied"
    );
    hub.stop().await;
}
