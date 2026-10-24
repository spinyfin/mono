use base64::Engine as _;
use boss_host_enrollment::{EnrollInput, HostCredentials, Policy, SystemClock};
use boss_protocol_spoke::v1::{
    self, hub_frame, spoke_frame,
    spoke_hub_server::{SpokeHub, SpokeHubServer},
};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, SanType,
};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{
    Request, Response, Status, Streaming,
    transport::{Certificate, Identity, Server, ServerTlsConfig},
};

pub const STEP: Duration = Duration::from_secs(20);

fn pem(label: &str, der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

pub struct Connection {
    pub hello: v1::Hello,
    pub incoming: Streaming<v1::SpokeFrame>,
    pub outgoing: mpsc::Sender<Result<v1::HubFrame, Status>>,
}

impl Connection {
    pub async fn send(&self, body: hub_frame::Body) {
        self.outgoing
            .send(Ok(v1::HubFrame {
                connection_generation: 1,
                body: Some(body),
            }))
            .await
            .unwrap();
    }
    pub async fn welcome(&self, through: u64) {
        self.send(hub_frame::Body::Welcome(v1::Welcome {
            epoch: 1,
            features: boss_spoke_outbound::contracts().features,
            hub_id: "hub".into(),
            authority_generation: "authority".into(),
            connection_generation: 1,
            received_records: Some(v1::Cursor {
                journal_id: self.hello.journal_id.clone(),
                through,
            }),
            command_journal_id: "commands".into(),
            ..Default::default()
        }))
        .await;
    }
    pub async fn next(&mut self) -> spoke_frame::Body {
        tokio::time::timeout(STEP, self.incoming.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .body
            .unwrap()
    }
    pub async fn receipt(&self, through: u64) {
        self.send(hub_frame::Body::RecordReceipt(v1::Cursor {
            journal_id: self.hello.journal_id.clone(),
            through,
        }))
        .await;
    }
}

struct Hub {
    connections: mpsc::Sender<Connection>,
    /// Connects still to be shed with `ResourceExhausted` before one is admitted.
    refusals: AtomicUsize,
}
#[tonic::async_trait]
impl SpokeHub for Hub {
    type ConnectStream = ReceiverStream<Result<v1::HubFrame, Status>>;
    async fn connect(
        &self,
        request: Request<Streaming<v1::SpokeFrame>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        assert!(request.peer_certs().is_some(), "mTLS client certificate required");
        if self
            .refusals
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Status::resource_exhausted("connection limit"));
        }
        let mut incoming = request.into_inner();
        let Some(spoke_frame::Body::Hello(hello)) = incoming.message().await?.and_then(|f| f.body) else {
            return Err(Status::invalid_argument("Hello required"));
        };
        let (outgoing, rx) = mpsc::channel(8);
        self.connections
            .send(Connection {
                hello,
                incoming,
                outgoing,
            })
            .await
            .unwrap();
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub endpoint: String,
    pub connections: mpsc::Receiver<Connection>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    pub async fn new() -> Self {
        Self::refusing(0).await
    }

    /// A hub that sheds the first `refusals` connects with `ResourceExhausted`.
    ///
    /// The client leaf is issued by the hub's real enrollment authority, so the
    /// daemon is exercised against the certificate shape production issues. The
    /// hub's server certificate comes from a separate CA the daemon trusts.
    pub async fn refusing(refusals: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let authority = dir.path().join("authority");
        std::fs::create_dir(&authority).unwrap();
        std::fs::set_permissions(&authority, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let credentials = HostCredentials::open(&authority, Policy::default(), Arc::new(SystemClock)).unwrap();
        credentials.authorize_host("host", "test").unwrap();
        let grant = credentials.issue_enrollment_token("host", "job").unwrap();
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let csr = CertificateParams::default().serialize_request(&key).unwrap();
        let issued = credentials
            .enroll(&EnrollInput {
                host_id: "host",
                job_id: "job",
                request_id: "request",
                token: grant.token.as_bytes(),
                csr_der: csr.der(),
            })
            .unwrap();
        let mut ca = CertificateParams::default();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let issuer = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
        std::fs::write(dir.path().join("ca.pem"), issuer.pem()).unwrap();
        std::fs::write(dir.path().join("cert.pem"), pem("CERTIFICATE", &issued.leaf_der)).unwrap();
        std::fs::write(dir.path().join("key.pem"), key.serialize_pem()).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&key, &issuer).unwrap();
        let (cert, key) = (cert.pem(), key.serialize_pem());
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(cert, key))
            .client_ca_root(Certificate::from_pem(credentials.trust_bundle_pem()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let (tx, connections) = mpsc::channel(8);
        let server = tokio::spawn(async move {
            Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(SpokeHubServer::new(Hub {
                    connections: tx,
                    refusals: AtomicUsize::new(refusals),
                }))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            dir,
            endpoint,
            connections,
            server,
        }
    }
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("state")
    }
    /// The daemon's own client configuration, for driving the session in process.
    pub fn endpoint(&self) -> tonic::transport::Endpoint {
        let read = |name: &str| std::fs::read(self.dir.path().join(name)).unwrap();
        let tls = tonic::transport::ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(read("ca.pem")))
            .identity(Identity::from_pem(read("cert.pem"), read("key.pem")));
        tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .unwrap()
            .tls_config(tls)
            .unwrap()
    }
    pub fn start(&self) -> Daemon {
        Daemon::start(self.dir.path(), &self.endpoint)
    }
    pub async fn connection(&mut self) -> Connection {
        tokio::time::timeout(STEP, self.connections.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

pub struct Daemon {
    child: Child,
    log: PathBuf,
}
impl Daemon {
    fn start(dir: &Path, endpoint: &str) -> Self {
        let log = dir.join("daemon.log");
        let child = Command::new(std::env::var("SPOKE_BIN").unwrap())
            .arg("--state-dir")
            .arg(dir.join("state"))
            .args(["--host-id", "host", "--hub", endpoint])
            .arg("--tls-dir")
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Self { child, log }
    }
    pub fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
    pub async fn failed(&mut self) -> String {
        tokio::time::timeout(STEP, async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    assert!(!status.success());
                    return std::fs::read_to_string(&self.log).unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
