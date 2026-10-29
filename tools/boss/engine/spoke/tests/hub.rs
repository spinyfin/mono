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
    collections::VecDeque,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{
    Request, Response, Status, Streaming,
    transport::{Certificate, Identity, Server, ServerTlsConfig},
};

pub const STEP: Duration = Duration::from_secs(20);

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
    pub fn welcome_body(&self, through: u64) -> v1::Welcome {
        v1::Welcome {
            epoch: 1,
            features: self.hello.features.clone(),
            hub_id: "hub".into(),
            authority_generation: "authority".into(),
            connection_generation: 1,
            received_records: Some(v1::Cursor {
                journal_id: self.hello.journal_id.clone(),
                through,
            }),
            command_journal_id: "commands".into(),
            ..Default::default()
        }
    }
    pub async fn welcome(&self, through: u64) {
        self.send(hub_frame::Body::Welcome(self.welcome_body(through))).await;
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
    /// Statuses the next connects are shed with, in order, before one is admitted.
    refusals: Mutex<VecDeque<tonic::Code>>,
    /// Journal identity from the Hello of every refused attempt.
    refused: Arc<Mutex<Vec<String>>>,
}
#[tonic::async_trait]
impl SpokeHub for Hub {
    type ConnectStream = ReceiverStream<Result<v1::HubFrame, Status>>;
    async fn connect(
        &self,
        request: Request<Streaming<v1::SpokeFrame>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        assert!(request.peer_certs().is_some(), "mTLS client certificate required");
        let mut incoming = request.into_inner();
        let Some(spoke_frame::Body::Hello(hello)) = incoming.message().await?.and_then(|f| f.body) else {
            return Err(Status::invalid_argument("Hello required"));
        };
        let refusal = self.refusals.lock().unwrap().pop_front();
        if let Some(code) = refusal {
            self.refused.lock().unwrap().push(hello.journal_id);
            return Err(Status::new(code, "refused before admission"));
        }
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
    /// Journal identity advertised by each refused attempt.
    pub refused: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    pub async fn new() -> Self {
        Self::shedding(vec![]).await
    }

    /// A hub that sheds the first `refusals` connects with `ResourceExhausted`.
    pub async fn refusing(refusals: usize) -> Self {
        Self::shedding(vec![tonic::Code::ResourceExhausted; refusals]).await
    }

    /// A hub that refuses the first connects with `codes`, in order, before
    /// admitting one.
    ///
    /// The client leaf is issued by the hub's real enrollment authority, so the
    /// daemon is exercised against the certificate shape production issues. The
    /// hub's server certificate comes from a separate CA the daemon trusts.
    pub async fn shedding(codes: Vec<tonic::Code>) -> Self {
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
        std::fs::write(
            dir.path().join("cert.pem"),
            boss_host_enrollment::pem_encode("CERTIFICATE", &issued.leaf_der),
        )
        .unwrap();
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
        let refused = Arc::new(Mutex::new(Vec::new()));
        let hub_refused = Arc::clone(&refused);
        let server = tokio::spawn(async move {
            Server::builder()
                .initial_stream_window_size(1024)
                .tls_config(tls)
                .unwrap()
                .add_service(SpokeHubServer::new(Hub {
                    connections: tx,
                    refusals: Mutex::new(codes.into()),
                    refused: hub_refused,
                }))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            dir,
            endpoint,
            connections,
            refused,
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
        Daemon::start(self.dir.path(), &self.endpoint, Path::new("cube"))
    }
    pub fn start_with_cube(&self, cube: &Path) -> Daemon {
        Daemon::start(self.dir.path(), &self.endpoint, cube)
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
    fn start(dir: &Path, endpoint: &str, cube: &Path) -> Self {
        let log = dir.join("daemon.log");
        let child = Command::new(std::env::var("SPOKE_BIN").unwrap())
            .arg("--cube-bin")
            .arg(cube)
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
