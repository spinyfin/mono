use super::*;
use boss_engine_runtime::hub::{
    HubStream,
    projection::{Projector, ProjectorConfig},
};
use boss_host_enrollment::{EnrollInput, HostCredentials, Policy, SystemClock};
use boss_host_execution::{
    process::process_snapshot,
    worker::{ExecutionRecord, ObservationConfig, Phase},
};
use boss_protocol_spoke::{
    negotiate::Contracts,
    v1::{hub_frame, spoke_frame, spoke_hub_client::SpokeHubClient},
};
use boss_spoke_journal::{Admission, Journal, Limits};
use boss_spoke_worker_api::Gateway;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Mutex, time::Duration};
use tokio::{
    net::{TcpListener, UnixListener},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

pub const HOST: &str = "request-host";
pub const PR: &str = "https://github.com/example/repo/pull/42";
const STEP: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct Probe(pub std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl boss_engine_merge_poller::MergeProbe for Probe {
    async fn probe(&self, url: &str) -> anyhow::Result<boss_engine_merge_poller::PrLifecycleProbe> {
        use boss_engine_merge_poller::*;
        assert_eq!(url, PR, "the hub must derive this execution's PR");
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(PrLifecycleProbe::builder()
            .url(url)
            .state(PrLifecycleState::Open(OpenPrStatus {
                mergeability: OpenPrMergeability::Clean,
                ci: OpenPrCiStatus::Clean,
            }))
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .raw_mergeable("MERGEABLE")
            .raw_merge_state_status("CLEAN")
            .head_ref_oid("abc123")
            .build())
    }
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct Fixture {
    pub probe: Arc<Probe>,
    pub state: Arc<ServerState>,
    pub execution_id: String,
    pub work_item_id: String,
    pub run: v1::Run,
    pub gateway: Gateway,
    pub hub: HubStream,
    pub incoming: tonic::Streaming<v1::HubFrame>,
    pub last_request: Option<v1::Record>,
    outgoing: mpsc::Sender<v1::SpokeFrame>,
    socket: PathBuf,
    sent: u64,
    command_journal: String,
    tasks: Vec<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Fixture {
    pub async fn new() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let probe = Arc::new(Probe::default());
        let (state, dir) =
            test_server_state_with_overrides(ServerStateOverrides::builder().merge_probe(probe.clone()).build());
        let (execution_id, work_item_id) = new_execution(&state, "Worker request target");
        let db = &state.work_db;
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
                rusqlite::params![PR, work_item_id],
            )
            .unwrap();
        db.add_host(HOST, "unused", 1, &[]).unwrap();
        db.set_host_transport(HOST, boss_work_db::HostTransport::SpokeReady)
            .unwrap();
        let assignment = db.reserve_spoke_assignment(&execution_id, HOST).unwrap();
        db.set_execution_pr_title_before(&execution_id, "The bound PR").unwrap();
        db.set_execution_pr_body_before(&execution_id, "Snapshot before execution")
            .unwrap();
        let run = v1::Run {
            execution_id: execution_id.clone(),
            run_id: assignment.run_id,
            assignment_generation: assignment.assignment_generation,
            spawn_token: assignment.spawn_token,
        };
        let credentials_dir = dir.path().join("credentials");
        std::fs::create_dir(&credentials_dir).unwrap();
        std::fs::set_permissions(&credentials_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let credentials =
            Arc::new(HostCredentials::open(&credentials_dir, Policy::default(), Arc::new(SystemClock)).unwrap());
        credentials.authorize_host(HOST, "test").unwrap();
        let grant = credentials.issue_enrollment_token(HOST, "job").unwrap();
        let key = KeyPair::generate().unwrap();
        let csr = CertificateParams::default().serialize_request(&key).unwrap();
        let issued = credentials
            .enroll(&EnrollInput {
                host_id: HOST,
                job_id: "job",
                request_id: "enroll",
                token: grant.token.as_bytes(),
                csr_der: csr.der(),
            })
            .unwrap();
        let client_identity = Identity::from_pem(
            boss_host_enrollment::pem_encode("CERTIFICATE", &issued.leaf_der),
            key.serialize_pem(),
        );
        let mut ca = CertificateParams::default();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let issuer = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&key, &issuer).unwrap();
        let server_identity = Identity::from_pem(cert.pem(), key.serialize_pem());
        let hub = HubStream::with_identity("hub", "authority", "test", credentials, (**db).clone());
        state.set_hub_stream(Some(hub.clone()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = hub.clone();
        let server_task = tokio::spawn(async move {
            server.serve(listener, server_identity).await.unwrap();
        });
        let channel = Endpoint::from_shared(format!("https://{address}"))
            .unwrap()
            .tls_config(
                ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(issuer.pem()))
                    .identity(client_identity),
            )
            .unwrap()
            .connect()
            .await
            .unwrap();
        let journal = Journal::open(&dir.path().join("spoke"), HOST, Limits::default()).unwrap();
        let journal_id = journal.identity().unwrap().1;
        let gateway = Gateway {
            journal: Arc::new(Mutex::new(journal)),
            state_dir: dir.path().join("spoke"),
            probe: Arc::new(|_| Box::pin(async { Ok((String::new(), 0)) })),
        };
        let execution = ExecutionRecord::builder()
            .execution_id(&execution_id)
            .run_id(&run.run_id)
            .spawn_token(&run.spawn_token)
            .assignment_generation(run.assignment_generation)
            .lease_id("lease")
            .workspace_id("workspace")
            .session("session")
            .phase(Phase::Alive)
            .boot_id("boot")
            .pid(std::process::id() as u64)
            .driver_id("claude")
            .observation(ObservationConfig {
                workspace: dir.path().into(),
                pending: dir.path().into(),
                file: None,
                pid_start: Some(process_snapshot(std::process::id() as i32).unwrap().start),
            })
            .build();
        gateway
            .journal
            .lock()
            .unwrap()
            .save_execution(&run.run_id, &run.spawn_token, &serde_json::to_vec(&execution).unwrap())
            .unwrap();
        let socket = dir.path().join("worker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let service = gateway.clone();
        let gateway_task = tokio::spawn(async move {
            service.serve(listener, || Ok(stamp())).await.unwrap();
        });
        let (outgoing, rx) = mpsc::channel(8);
        outgoing
            .send(v1::SpokeFrame {
                body: Some(spoke_frame::Body::Hello(v1::Hello {
                    host_id: HOST.into(),
                    journal_id,
                    boot_id: "boot".into(),
                    min_epoch: 1,
                    max_epoch: 1,
                    features: Contracts::first_release().features,
                    first_retained: 1,
                    instance_nonce: vec![1; 16],
                    ..Default::default()
                })),
            })
            .await
            .unwrap();
        let mut incoming = SpokeHubClient::new(channel)
            .connect(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let Some(hub_frame::Body::Welcome(welcome)) = incoming.message().await.unwrap().unwrap().body else {
            panic!("welcome")
        };
        Self::builder()
            .probe(probe)
            .state(state)
            .execution_id(execution_id)
            .work_item_id(work_item_id)
            .run(run)
            .gateway(gateway)
            .hub(hub)
            .incoming(incoming)
            .outgoing(outgoing)
            .socket(socket)
            .sent(0)
            .command_journal(welcome.command_journal_id)
            .tasks(vec![server_task, gateway_task])
            .dir(dir)
            .build()
    }

    pub fn start_cli(&self, args: &[&str], execution: &str) -> JoinHandle<std::process::Output> {
        let binary = std::env::current_dir()
            .unwrap()
            .join(std::env::var("BOSS_TEST_CLI").unwrap());
        let mut command = tokio::process::Command::new(binary);
        command
            .env("BOSS_SOCKET_PATH", &self.socket)
            .env("BOSS_RUN_ID", execution)
            .args(args)
            .arg("--json");
        tokio::spawn(async move { command.output().await.unwrap() })
    }

    pub async fn next_record(&mut self) -> v1::Record {
        let record = tokio::time::timeout(STEP, async {
            loop {
                if let Some(record) = self.gateway.journal.lock().unwrap().next_record(self.sent).unwrap() {
                    break record;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        self.sent = record.sequence;
        v1::Record::decode(record.bytes.as_slice()).unwrap()
    }

    pub async fn send_record(&self, record: v1::Record) {
        self.outgoing
            .send(v1::SpokeFrame {
                body: Some(spoke_frame::Body::Record(record)),
            })
            .await
            .unwrap();
    }

    pub async fn next_frame(&mut self) -> Result<Option<v1::HubFrame>, Box<tonic::Status>> {
        tokio::time::timeout(STEP, async {
            loop {
                let frame = self.incoming.message().await?;
                if let Some(v1::HubFrame {
                    body: Some(hub_frame::Body::Pulse(pulse)),
                    ..
                }) = &frame
                {
                    self.outgoing
                        .send(v1::SpokeFrame {
                            body: Some(spoke_frame::Body::PulseReply(v1::Pulse {
                                nonce: pulse.nonce.clone(),
                                stamp: Some(stamp()),
                            })),
                        })
                        .await
                        .unwrap();
                } else {
                    return Ok(frame);
                }
            }
        })
        .await
        .unwrap()
    }

    pub async fn expect_receipt(&mut self) {
        let frame = self.next_frame().await.unwrap().unwrap();
        assert!(
            matches!(frame.body, Some(hub_frame::Body::RecordReceipt(_))),
            "{frame:?}"
        );
    }

    pub async fn receive_next(&mut self) {
        let record = self.next_record().await;
        if matches!(record.body, Some(record::Body::Request(_))) {
            self.last_request = Some(record.clone());
        }
        self.send_record(record).await;
        self.expect_receipt().await;
    }

    pub fn projector(&self) -> Projector {
        self.hub.projector(
            (*self.state.work_db).clone(),
            crate::app::spoke_projection::applier(&self.state),
            ProjectorConfig::default(),
        )
    }

    pub fn append(&self, body: record::Body) {
        self.gateway
            .journal
            .lock()
            .unwrap()
            .append(
                v1::Record {
                    run: Some(self.run.clone()),
                    observed: Some(stamp()),
                    body: Some(body),
                    ..Default::default()
                },
                Admission::Lifecycle,
            )
            .unwrap();
    }

    pub async fn project_and_reply(&mut self) {
        let projector = self.projector();
        let project = tokio::spawn(async move { projector.drain().await });
        let frame = self.next_frame().await.unwrap().unwrap();
        let Some(hub_frame::Body::Command(command)) = frame.body else {
            panic!("command")
        };
        self.gateway.apply(&command).unwrap();
        // A duplicate reply is harmless, including before the CLI consumes it.
        self.gateway.apply(&command).unwrap();
        self.outgoing
            .send(v1::SpokeFrame {
                body: Some(spoke_frame::Body::CommandReceipt(v1::Cursor {
                    journal_id: self.command_journal.clone(),
                    through: command.sequence,
                })),
            })
            .await
            .unwrap();
        self.append(record::Body::Result(v1::CommandResult {
            command_id: command.command_id,
            code: v1::ResultCode::Ok as i32,
            ..Default::default()
        }));
        self.receive_next().await;
        project.await.unwrap();
    }

    pub async fn read(&mut self, args: &[&str]) -> Value {
        let cli = self.start_cli(args, &self.execution_id);
        self.receive_next().await;
        self.project_and_reply().await;
        let output = cli.await.unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("Spoke hub: revision"));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    pub fn effect_payloads(&self) -> Vec<Vec<u8>> {
        let conn = self.state.work_db.connect().unwrap();
        let mut query = conn
            .prepare("SELECT payload FROM spoke_effects WHERE effect_kind = 'worker_reply' ORDER BY sequence")
            .unwrap();
        query
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
}

fn stamp() -> v1::Stamp {
    v1::Stamp {
        boot_id: "boot".into(),
        wall_unix_ms: 1_000,
        ..Default::default()
    }
}

pub fn denied(output: std::process::Output, reason: &str) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(reason), "{stderr}");
}
