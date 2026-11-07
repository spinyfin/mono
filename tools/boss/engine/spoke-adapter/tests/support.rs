//! A real hub (stream manager over mTLS gRPC, work database, command outbox)
//! and a scripted spoke on the other end of a real stream.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use boss_execution_contracts::{HostAdapter, HostAdapterProvider, RunOutcome, RunWaitState};
use boss_host_enrollment::{AuthorizedHost, CredentialEvent, Denial, HostAuthorizer};
use boss_protocol::{ExecutionKind, ExecutionStatus, Task, TaskKind, TaskStatus, WorkExecution, WorkItem};
use boss_protocol_spoke::{
    negotiate::{Contracts, Selection},
    v1::*,
};
use boss_spoke_adapter::{
    ComposedSpawn, SpawnComposer, SpawnInput, SpokeAdapterConfig, SpokeHostAdapterProvider, WorkDbOutbox,
};
use boss_spoke_stream::{
    Config, StreamManager,
    store::{Admission, Session, SpokeObservationSink, StoreError},
};
use boss_work_db::{Host, HostOutboxFault, HostTransport, SpokeRunKey, WorkDb};
use prost::Message;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use rustls::pki_types::CertificateDer;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig};

pub const STEP: Duration = Duration::from_secs(10);
pub const HOST: &str = "host";

struct Authority {
    events: broadcast::Sender<CredentialEvent>,
    leaf: Vec<u8>,
}
impl HostAuthorizer for Authority {
    fn authorize(&self, chain: &[CertificateDer<'_>]) -> Result<AuthorizedHost, Denial> {
        if !chain.first().is_some_and(|c| c.as_ref() == self.leaf) {
            return Err(Denial::Untrusted);
        }
        Ok(AuthorizedHost {
            host_id: HOST.into(),
            serial: "serial".into(),
            valid_until: SystemTime::now() + STEP * 100,
        })
    }
    fn revalidate(&self, identity: &AuthorizedHost) -> Result<AuthorizedHost, Denial> {
        Ok(identity.clone())
    }
    fn subscribe(&self) -> broadcast::Receiver<CredentialEvent> {
        self.events.subscribe()
    }
}

/// The ingress half of the hub as the observation task will provide it: it
/// settles a command result in the same place it records the receipt.
struct Sink {
    db: Arc<WorkDb>,
    state: Mutex<(u64, Option<String>, u64)>,
}
#[tonic::async_trait]
impl SpokeObservationSink for Sink {
    async fn open(&self, hello: &Hello, _: &Selection) -> Result<Admission, StoreError> {
        let mut state = self.state.lock().unwrap();
        if state.1.as_ref().is_some_and(|id| id != &hello.journal_id) {
            return Err(StoreError::ReplayGap);
        }
        state.1 = Some(hello.journal_id.clone());
        state.0 += 1;
        Ok(Admission {
            generation: state.0,
            received: Cursor {
                journal_id: hello.journal_id.clone(),
                through: state.2,
            },
        })
    }
    async fn commit(&self, session: &Session, _raw: &[u8], record: &Record) -> Result<Cursor, StoreError> {
        let mut state = self.state.lock().unwrap();
        if session.generation != state.0 {
            return Err(StoreError::Denied);
        }
        if record.sequence > state.2 {
            if record.sequence != state.2 + 1 {
                return Err(StoreError::ReplayGap);
            }
            if let Some(record::Body::Result(result)) = &record.body {
                let run = record.run.as_ref().map(|run| SpokeRunKey {
                    execution_id: run.execution_id.clone(),
                    assignment_generation: run.assignment_generation,
                    run_id: run.run_id.clone(),
                    spawn_token: run.spawn_token.clone(),
                });
                self.db
                    .settle_host_command(
                        &session.host_id,
                        &result.command_id,
                        run.as_ref(),
                        &result.encode_to_vec(),
                    )
                    .map_err(|error| match error.downcast_ref::<HostOutboxFault>() {
                        Some(_) => StoreError::Denied,
                        None => StoreError::Unavailable,
                    })?;
            }
            state.2 = record.sequence;
        }
        Ok(Cursor {
            journal_id: session.journal_id.clone(),
            through: state.2,
        })
    }
}

/// Stands in for the SSH provider: any use of it is a failure of routing.
struct Legacy;
#[async_trait]
impl HostAdapterProvider for Legacy {
    async fn adapter_for(&self, _: &Host) -> anyhow::Result<Arc<dyn HostAdapter>> {
        anyhow::bail!("the legacy transport was used")
    }
}

struct Composer;
#[async_trait]
impl SpawnComposer for Composer {
    async fn compose(&self, _: SpawnInput<'_>) -> anyhow::Result<ComposedSpawn> {
        Ok(ComposedSpawn {
            spawn: Spawn {
                driver_id: "claude".into(),
                model: "model".into(),
                prompt_utf8: b"do the work".to_vec(),
                settings_json: b"{}".to_vec(),
                ..Default::default()
            },
            outcome: RunOutcome {
                wait_state: RunWaitState::WorkerPaneAlive,
                result_summary: None,
                attention: None,
                slot_id: None,
                spawn_config: None,
            },
        })
    }
}

#[derive(bon::Builder)]
pub struct Fixture {
    pub db: Arc<WorkDb>,
    pub provider: SpokeHostAdapterProvider,
    pub manager: StreamManager,
    dialer: Endpoint,
    server: tokio::task::JoinHandle<()>,
    #[builder(name = dir)]
    _dir: tempfile::TempDir,
}

impl Fixture {
    pub async fn new() -> Self {
        Self::with(
            SpokeAdapterConfig::builder()
                .command_deadline(Duration::from_secs(5))
                .build(),
        )
        .await
    }

    pub async fn with(config: SpokeAdapterConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("work.db")).unwrap());
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_identity = Identity::from_pem(params.signed_by(&key, &ca).unwrap().pem(), key.serialize_pem());
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::URI(format!("urn:boss:host:{HOST}").try_into().unwrap())];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let leaf = params.signed_by(&key, &ca).unwrap();
        let identity = Identity::from_pem(leaf.pem(), key.serialize_pem());
        let auth = Arc::new(Authority {
            events: broadcast::channel(4).0,
            leaf: leaf.der().to_vec(),
        });
        let sink = Arc::new(Sink {
            db: Arc::clone(&db),
            state: Mutex::new((0, None, 0)),
        });
        let outbox = WorkDbOutbox::new(Arc::clone(&db));
        let manager = StreamManager::new(
            Config::builder()
                .hub_id("hub")
                .authority_generation("authority")
                .build_fingerprint("test")
                .max_hosts(4)
                .max_pending(8)
                .hello_timeout(Duration::from_secs(2))
                .pulse_interval(Duration::from_secs(2))
                .pulse_timeout(Duration::from_secs(1))
                .build(),
            auth,
            sink,
            Arc::new(outbox),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = ServerTlsConfig::new()
            .identity(server_identity)
            .client_ca_root(Certificate::from_pem(ca.pem()))
            .client_auth_optional(true);
        let service = manager.service();
        let server = tokio::spawn(async move {
            Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca.pem()))
            .domain_name("localhost")
            .identity(identity);
        let dialer = Endpoint::from_shared(format!("https://{addr}"))
            .unwrap()
            .tls_config(tls)
            .unwrap();
        let provider = SpokeHostAdapterProvider::new(
            Arc::new(Legacy),
            manager.clone(),
            Arc::clone(&db),
            Arc::new(Composer),
            config,
        );
        Self::builder()
            .db(db)
            .provider(provider)
            .manager(manager)
            .dialer(dialer)
            .server(server)
            .dir(dir)
            .build()
    }

    pub fn host(&self, id: &str) -> Host {
        Host::builder()
            .id(id)
            .pool_size(1)
            .enabled(true)
            .consecutive_failures(0)
            .created_at("0")
            .build()
    }

    pub fn ready(&self, id: &str) {
        self.db.set_host_transport(id, HostTransport::SpokeReady).unwrap();
    }

    /// The adapter an execution is dispatched through, reserving its assignment.
    pub async fn adapter(&self, host: &str, execution_id: &str) -> anyhow::Result<Arc<dyn HostAdapter>> {
        self.provider
            .adapter_for_execution(&self.host(host), execution_id)
            .await
    }

    /// Connect a scripted spoke that has already produced `last_record` records.
    pub async fn spoke(&self, script: Script, command_cursor: Option<Cursor>, last_record: u64) -> Peer {
        let (tx, rx) = mpsc::channel(32);
        tx.send(SpokeFrame {
            body: Some(spoke_frame::Body::Hello(Hello {
                host_id: HOST.into(),
                journal_id: "journal".into(),
                boot_id: "boot".into(),
                min_epoch: 1,
                max_epoch: 1,
                features: Contracts::first_release().features,
                instance_nonce: vec![1; 16],
                first_retained: 1,
                last_record,
                received_commands: command_cursor,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let channel = tokio::time::timeout(STEP, self.dialer.connect())
            .await
            .unwrap()
            .unwrap();
        let mut incoming = tokio::time::timeout(
            STEP,
            spoke_hub_client::SpokeHubClient::new(channel).connect(ReceiverStream::new(rx)),
        )
        .await
        .unwrap()
        .unwrap()
        .into_inner();
        let Some(hub_frame::Body::Welcome(welcome)) = incoming.message().await.unwrap().unwrap().body else {
            panic!("expected a welcome")
        };
        let seen: Arc<Mutex<Vec<Command>>> = Arc::default();
        let next_record = Arc::new(AtomicU64::new(last_record + 1));
        let journal = welcome.command_journal_id;
        let script = Arc::new(Mutex::new(script));
        let (task_seen, task_next, task_journal, task_tx) =
            (Arc::clone(&seen), Arc::clone(&next_record), journal.clone(), tx.clone());
        let task = tokio::spawn(async move {
            let (seen, next_record, journal, tx) = (task_seen, task_next, task_journal, task_tx);
            while let Ok(Some(frame)) = incoming.message().await {
                match frame.body {
                    Some(hub_frame::Body::Pulse(pulse)) => {
                        let _ = tx.send(frame_of(spoke_frame::Body::PulseReply(pulse))).await;
                    }
                    Some(hub_frame::Body::Command(command)) => {
                        seen.lock().unwrap().push(command.clone());
                        let _ = tx
                            .send(frame_of(spoke_frame::Body::CommandReceipt(Cursor {
                                journal_id: journal.clone(),
                                through: command.sequence,
                            })))
                            .await;
                        let reply = (script.lock().unwrap())(&command);
                        if let Some(result) = reply {
                            let sequence = next_record.fetch_add(1, Ordering::SeqCst);
                            let _ = tx
                                .send(frame_of(spoke_frame::Body::Record(result_record(
                                    sequence,
                                    command.run.clone(),
                                    result,
                                ))))
                                .await;
                        }
                    }
                    _ => {}
                }
            }
        });
        Peer {
            tx,
            seen,
            next_record,
            journal,
            task,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn frame_of(body: spoke_frame::Body) -> SpokeFrame {
    SpokeFrame { body: Some(body) }
}

fn result_record(sequence: u64, run: Option<Run>, result: CommandResult) -> Record {
    Record {
        sequence,
        observed: Some(Stamp {
            boot_id: "boot".into(),
            ..Default::default()
        }),
        run,
        body: Some(record::Body::Result(result)),
    }
}

/// How the scripted spoke answers a command: `None` stays silent.
pub type Script = Box<dyn FnMut(&Command) -> Option<CommandResult> + Send>;

pub struct Peer {
    tx: mpsc::Sender<SpokeFrame>,
    seen: Arc<Mutex<Vec<Command>>>,
    next_record: Arc<AtomicU64>,
    pub journal: String,
    task: tokio::task::JoinHandle<()>,
}

impl Peer {
    /// Drop the stream, as a lost network or a crashed daemon would.
    pub fn disconnect(self) {
        self.task.abort();
    }

    pub fn commands(&self) -> Vec<Command> {
        self.seen.lock().unwrap().clone()
    }

    pub async fn wait_for_command(&self, count: usize) -> Vec<Command> {
        let deadline = tokio::time::Instant::now() + STEP;
        loop {
            let seen = self.commands();
            if seen.len() >= count {
                return seen;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "spoke saw {} of {count} commands",
                seen.len()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Answer a command the spoke chose not to answer in time.
    pub async fn answer_late(&self, command: &Command, result: CommandResult) {
        let sequence = self.next_record.fetch_add(1, Ordering::SeqCst);
        self.tx
            .send(frame_of(spoke_frame::Body::Record(result_record(
                sequence,
                command.run.clone(),
                result,
            ))))
            .await
            .unwrap();
    }
}

pub fn ok(command: &Command, value: Option<command_result::Value>) -> CommandResult {
    CommandResult {
        command_id: command.command_id.clone(),
        code: ResultCode::Ok.into(),
        value,
        ..Default::default()
    }
}

pub fn refused(command: &Command, code: ResultCode, reason: &str) -> CommandResult {
    CommandResult {
        command_id: command.command_id.clone(),
        code: code.into(),
        reason: reason.into(),
        ..Default::default()
    }
}

pub fn alive() -> PaneState {
    PaneState {
        life: PaneLife::Alive.into(),
        session: "boss-token".into(),
        pane_id: "%1".into(),
        pid: 4242,
        boot_id: "boot".into(),
        ..Default::default()
    }
}

pub fn pane(life: PaneLife) -> command_result::Value {
    command_result::Value::Pane(PaneState {
        life: life.into(),
        ..alive()
    })
}

/// A workspace reply for the command's operation: the lease the host grants.
pub fn workspace(command: &Command, json: serde_json::Value) -> command_result::Value {
    let Some(command::Body::Workspace(op)) = &command.body else {
        panic!("not a workspace command: {command:?}")
    };
    command_result::Value::Workspace(WorkspaceResult {
        repo_id: op.repo_id.clone(),
        workspace_id: json["workspace_id"].as_str().unwrap_or(&op.workspace_id).to_owned(),
        lease_id: json["lease_id"].as_str().unwrap_or(&op.lease_id).to_owned(),
        display_path: "/hosts/workspaces/worker".into(),
        cube_result_json: serde_json::to_vec(&json).unwrap(),
        ..Default::default()
    })
}

pub fn lease_json() -> serde_json::Value {
    serde_json::json!({"workspace_id": "worker", "lease_id": "lease-1", "workspace_path": "/hosts/workspaces/worker"})
}

pub fn execution(id: &str) -> WorkExecution {
    WorkExecution::builder()
        .id(id)
        .work_item_id("chore_1")
        .kind(ExecutionKind::TaskImplementation)
        .status(ExecutionStatus::Running)
        .repo_remote_url("https://github.com/acme/widget")
        .created_at("1700000000")
        .cube_lease_id("lease-1")
        .build()
}

pub fn work_item() -> WorkItem {
    WorkItem::Chore(
        Task::builder()
            .id("task_x")
            .product_id("prod")
            .kind(TaskKind::Chore)
            .name("x")
            .description("x")
            .status(TaskStatus::Active)
            .created_at("2026-01-01T00:00:00Z")
            .updated_at("2026-01-01T00:00:00Z")
            .build(),
    )
}
