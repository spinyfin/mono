use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use boss_host_enrollment::HostCredentials;
use boss_protocol_spoke::v1::enrollment_client::EnrollmentClient;
use boss_protocol_spoke::v1::spoke_hub_client::SpokeHubClient;
use boss_protocol_spoke::v1::{self, EnrollRequest, ResultCode, spoke_frame};
use boss_spoke_hub::HubStream;
use boss_spoke_stream::store::{CommandAdmission, CommandJournal, CommandOutbox, PendingCommand, StoreError};
use boss_spoke_upgrade::{UpgradeController, decode_sha256, upgrade_command_id};
use boss_work_db::{HostUpgradeState, WorkDb};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::support::*;

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

/// Fault switch for outbox reads, wrapped around the real outbox by the test.
#[derive(Default)]
struct ReadFaults(std::sync::atomic::AtomicBool);

impl ReadFaults {
    fn fail(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct FlakyOutbox {
    inner: Arc<dyn CommandOutbox>,
    faults: Arc<ReadFaults>,
}

#[tonic::async_trait]
impl CommandOutbox for FlakyOutbox {
    async fn resume(&self, host: &str, receipt: Option<&v1::Cursor>) -> Result<CommandJournal, StoreError> {
        self.inner.resume(host, receipt).await
    }
    async fn append(
        &self,
        host: &str,
        admission: CommandAdmission,
        command: v1::Command,
    ) -> Result<v1::Command, StoreError> {
        self.inner.append(host, admission, command).await
    }
    async fn next(&self, host: &str, after: u64) -> Result<Option<PendingCommand>, StoreError> {
        self.inner.next(host, after).await
    }
    async fn reject(&self, host: &str, sequence: u64, reason: &str) -> Result<v1::CommandResult, StoreError> {
        self.inner.reject(host, sequence, reason).await
    }
    async fn receipt(&self, host: &str, receipt: &v1::Cursor) -> Result<(), StoreError> {
        self.inner.receipt(host, receipt).await
    }
    async fn command(&self, host: &str, command_id: &str) -> Result<Option<v1::Command>, StoreError> {
        if self.faults.0.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        self.inner.command(host, command_id).await
    }
    async fn completed(&self, host: &str, command_id: &str) -> Result<Option<v1::CommandResult>, StoreError> {
        if self.faults.0.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        self.inner.completed(host, command_id).await
    }
}

async fn start(db: WorkDb, credentials: Arc<HostCredentials>) -> Running {
    start_with_outbox(db, credentials, None).await
}

async fn start_with_outbox(db: WorkDb, credentials: Arc<HostCredentials>, faults: Option<Arc<ReadFaults>>) -> Running {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pki = ServerPki::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let identity = pki.identity();
    let stream = match faults {
        None => hub_stream(db, credentials),
        Some(faults) => HubStream::with_outbox(hub_config(), credentials, db, move |inner| {
            Arc::new(FlakyOutbox { inner, faults })
        }),
    };
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

async fn dial(hub: &Running, identity: tonic::transport::Identity) -> Channel {
    let tls = ClientTlsConfig::new()
        .ca_certificate(hub.pki.trusted())
        .domain_name("localhost")
        .identity(identity);
    Endpoint::from_shared(format!("https://127.0.0.1:{}", hub.addr.port()))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .expect("dial")
}

async fn connect(channel: Channel, hello: v1::Hello) -> Peer {
    let (tx, rx) = mpsc::channel(8);
    tx.send(v1::SpokeFrame {
        body: Some(spoke_frame::Body::Hello(hello)),
    })
    .await
    .unwrap();
    let rx = tokio::time::timeout(STEP, SpokeHubClient::new(channel).connect(ReceiverStream::new(rx)))
        .await
        .unwrap()
        .expect("connect")
        .into_inner();
    let mut peer = Peer {
        tx,
        rx,
        next_sequence: 1,
    };
    match peer.next().await {
        Some(v1::hub_frame::Body::Welcome(_)) => {}
        other => panic!("expected welcome, got {other:?}"),
    }
    peer
}

async fn enroll_and_connect(hub: &Running, credentials: &HostCredentials, host_id: &str) -> Peer {
    let identity = enroll_identity(hub, credentials, host_id).await;
    connect(dial(hub, identity).await, hello(host_id)).await
}

async fn enroll_identity(hub: &Running, credentials: &HostCredentials, host_id: &str) -> tonic::transport::Identity {
    credentials.authorize_host(host_id, "test").expect("authorize");
    let grant = credentials.issue_enrollment_token(host_id, "job").expect("token");
    let spoke = Spoke::new();
    let mut anonymous = EnrollmentClient::new(
        Endpoint::from_shared(format!("https://127.0.0.1:{}", hub.addr.port()))
            .unwrap()
            .tls_config(
                ClientTlsConfig::new()
                    .ca_certificate(hub.pki.trusted())
                    .domain_name("localhost"),
            )
            .unwrap()
            .connect()
            .await
            .expect("anonymous dial"),
    );
    let cert = anonymous
        .enroll(EnrollRequest {
            host_id: host_id.into(),
            job_id: "job".into(),
            request_id: format!("req-{host_id}"),
            token: grant.token.as_bytes().to_vec(),
            csr_der: spoke.csr(),
        })
        .await
        .expect("enroll")
        .into_inner();
    spoke.identity(&cert)
}

fn rollout(db: &WorkDb, hub: &Running, catalog: Option<boss_spoke_upgrade::SpokePackageCatalog>) -> UpgradeController {
    controller(db.clone(), hub.stream.manager().clone(), catalog)
}

fn shared(controller: UpgradeController) -> Arc<UpgradeController> {
    Arc::new(controller)
}

async fn tick_offer(controller: &Arc<UpgradeController>, peer: &mut Peer) -> v1::Command {
    let ticking = {
        let controller = Arc::clone(controller);
        tokio::spawn(async move { controller.tick().await })
    };
    let command = peer.accept_command(ResultCode::Ok, "").await;
    ticking.await.expect("join").expect("tick");
    command
}

async fn tick_offer_rejected(controller: &Arc<UpgradeController>, peer: &mut Peer, reason: &str) -> v1::Command {
    let ticking = {
        let controller = Arc::clone(controller);
        tokio::spawn(async move { controller.tick().await })
    };
    let command = peer.accept_command(ResultCode::Rejected, reason).await;
    ticking.await.expect("join").expect("tick");
    command
}

async fn tick(controller: &Arc<UpgradeController>) {
    controller.tick().await.expect("tick");
}

#[tokio::test(flavor = "multi_thread")]
async fn outdated_spoke_is_offered_the_hub_release_on_connect() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut peer = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    peer.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    let command = tick_offer(&controller, &mut peer).await;
    let upgrade = upgrade_of(&command);
    assert_eq!(command.command_id, upgrade_command_id("edge-a", DESIRED));
    assert_eq!(upgrade.release_id, DESIRED);
    assert_eq!(upgrade.manifest_url, MANIFEST_URL);
    assert_eq!(upgrade.manifest_sha256, decode_sha256(DIGEST_HEX).unwrap());

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_host_at_a_time_waits_for_health() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    let command = tick_offer(&controller, &mut first).await;
    assert_eq!(command.command_id, upgrade_command_id("edge-a", DESIRED));
    second.expect_no_command().await;

    tick(&controller).await;
    second.expect_no_command().await;

    first
        .report_inventory(DESIRED, "ok", "", &upgrade_command_id("edge-a", DESIRED))
        .await;
    let second_command = tick_offer(&controller, &mut second).await;
    assert_eq!(second_command.command_id, upgrade_command_id("edge-b", DESIRED));

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_upgrade_halts_rollout_and_shows_on_host_status() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    let command = tick_offer(&controller, &mut first).await;
    first
        .report_inventory("1.0.1", "manifest_sig", "bad signature", &command.command_id)
        .await;
    tick(&controller).await;

    let view = db.host_upgrade_view("edge-a").unwrap().unwrap();
    assert_eq!(view.state, HostUpgradeState::Failed);
    assert_eq!(view.last_upgrade_result.as_deref(), Some("manifest_sig"));
    assert_eq!(view.desired_release.as_deref(), Some(DESIRED));
    let rollout_row = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(rollout_row.halted);
    assert_eq!(rollout_row.halt_host_id.as_deref(), Some("edge-a"));

    tick(&controller).await;
    second.expect_no_command().await;

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_upgrade_halts_without_offering_the_next_host() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    tick_offer_rejected(&controller, &mut first, "upgrade_busy").await;
    let rollout_row = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(rollout_row.halted);
    tick(&controller).await;
    second.expect_no_command().await;

    let view = db.host_upgrade_view("edge-a").unwrap().unwrap();
    assert_eq!(view.state, HostUpgradeState::Failed);
    assert_eq!(view.last_upgrade_result.as_deref(), Some("refused"));
    assert_eq!(view.last_upgrade_error.as_deref(), Some("upgrade_busy"));
    assert!(view.last_upgrade_at.is_some());

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_helper_failure_does_not_halt_a_new_offer() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first
        .report_inventory("1.0.1", "manifest_sig", "old", "upgrade-edge-a-1.0.1")
        .await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    tick_offer(&controller, &mut first).await;
    tick(&controller).await;
    second.expect_no_command().await;
    let rollout_row = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(
        !rollout_row.halted,
        "stale helper failure must not halt the new command"
    );

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn new_running_release_without_helper_ok_does_not_advance() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    let command = tick_offer(&controller, &mut first).await;
    first.report_inventory(DESIRED, "", "", &command.command_id).await;
    tick(&controller).await;
    second.expect_no_command().await;

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn host_without_a_catalog_package_does_not_block_the_fleet() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut arm = enroll_and_connect(&hub, &fx.credentials, "edge-arm").await;
    let mut amd = enroll_and_connect(&hub, &fx.credentials, "edge-amd").await;
    arm.report_inventory_on("linux", "aarch64", "1.0.1", "", "", "").await;
    amd.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    let command = tick_offer(&controller, &mut amd).await;
    assert_eq!(command.command_id, upgrade_command_id("edge-amd", DESIRED));
    arm.expect_no_command().await;
    let view = db.host_upgrade_view("edge-arm").unwrap().unwrap();
    assert_eq!(view.state, HostUpgradeState::Unpublished);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_before_append_resubmits_the_saved_command() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut peer = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    peer.report_inventory("1.0.1", "", "", "").await;

    let command_id = upgrade_command_id("edge-a", DESIRED);
    let command = v1::Command {
        command_id: command_id.clone(),
        not_after_unix_ms: 4_102_444_800_000,
        body: Some(v1::command::Body::Upgrade(v1::Upgrade {
            release_id: DESIRED.into(),
            manifest_url: MANIFEST_URL.into(),
            manifest_sha256: decode_sha256(DIGEST_HEX).unwrap(),
        })),
        ..Default::default()
    };
    let mut row = boss_work_db::SpokeUpgradeRollout::for_release(DESIRED, true);
    row.in_flight_host_id = Some("edge-a".into());
    row.in_flight_command_id = Some(command_id);
    row.in_flight_release = Some(DESIRED.into());
    row.in_flight_command = Some(prost::Message::encode_to_vec(&command));
    db.save_spoke_upgrade_rollout(&row).unwrap();

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    let offered = tick_offer(&controller, &mut peer).await;
    assert_eq!(offered.command_id, upgrade_command_id("edge-a", DESIRED));

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delayed_refusal_is_read_from_the_durable_outbox() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(
        UpgradeController::new(
            db.clone(),
            hub.stream.manager().clone(),
            DESIRED,
            Arc::new(boss_spoke_upgrade::StaticCatalog(Some(published_catalog()))),
        )
        .with_command_deadline(Duration::from_millis(80)),
    );
    let ticking = {
        let controller = Arc::clone(&controller);
        tokio::spawn(async move { controller.tick().await })
    };
    let command = first.take_command().await;
    ticking.await.expect("join").expect("tick");
    first
        .send_record(result_record(
            first.next_sequence,
            &command.command_id,
            ResultCode::Rejected,
            "upgrade_busy",
        ))
        .await;
    first.next_sequence += 1;
    tick(&controller).await;
    let rollout_row = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(rollout_row.halted);
    second.expect_no_command().await;

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn desired_release_change_waits_for_the_outstanding_offer() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let first_controller = shared(rollout(&db, &hub, Some(published_catalog())));
    tick_offer(&first_controller, &mut first).await;

    let mut next_catalog = published_catalog();
    next_catalog.release_id = "1.0.10".into();
    let next = shared(
        UpgradeController::new(
            db.clone(),
            hub.stream.manager().clone(),
            "1.0.10",
            Arc::new(boss_spoke_upgrade::StaticCatalog(Some(next_catalog))),
        )
        .with_command_deadline(Duration::from_millis(80)),
    );
    tick(&next).await;
    second.expect_no_command().await;
    let outstanding = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert_eq!(outstanding.in_flight_host_id.as_deref(), Some("edge-a"));
    assert_eq!(
        outstanding.in_flight_command_id.as_deref(),
        Some("upgrade-edge-a-1.0.9")
    );
    assert_eq!(outstanding.desired_release, "1.0.10");
    first
        .report_inventory(DESIRED, "ok", "", &upgrade_command_id("edge-a", DESIRED))
        .await;
    tick(&next).await;
    let settled = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(
        settled
            .in_flight_command_id
            .as_deref()
            .is_some_and(|id| id.ends_with("-1.0.10")),
        "after the outstanding offer settles, the new release is offered one host at a time, got {:?}",
        settled.in_flight_command_id
    );
    assert!(!settled.halted);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn in_flight_upgrade_times_out_while_connected() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(
        UpgradeController::new(
            db.clone(),
            hub.stream.manager().clone(),
            DESIRED,
            Arc::new(boss_spoke_upgrade::StaticCatalog(Some(published_catalog()))),
        )
        .with_upgrade_timeout(Duration::ZERO),
    );
    tick_offer(&controller, &mut first).await;
    tick(&controller).await;
    let rollout_row = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(rollout_row.halted);
    assert_eq!(rollout_row.halt_reason.as_deref(), Some("upgrade_timeout"));
    second.expect_no_command().await;

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_host_is_offered_when_it_reconnects() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    first.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    tick_offer(&controller, &mut first).await;
    first
        .report_inventory(DESIRED, "ok", "", &upgrade_command_id("edge-a", DESIRED))
        .await;
    tick(&controller).await;

    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    second.report_inventory("1.0.1", "", "", "").await;
    let command = tick_offer(&controller, &mut second).await;
    assert_eq!(command.command_id, upgrade_command_id("edge-b", DESIRED));

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_offered_when_the_hub_release_has_no_packages() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut peer = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    peer.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, None));
    tick(&controller).await;
    peer.expect_no_command().await;
    let view = db.host_upgrade_view("edge-a").unwrap().unwrap();
    assert_eq!(view.state, HostUpgradeState::Unpublished);
    assert_eq!(view.desired_release.as_deref(), Some(DESIRED));

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn up_to_date_spoke_is_left_alone() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut peer = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    peer.report_inventory(DESIRED, "ok", "", "prev").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    tick(&controller).await;
    peer.expect_no_command().await;
    let view = db.host_upgrade_view("edge-a").unwrap().unwrap();
    assert_eq!(view.state, HostUpgradeState::Current);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn desired_release_change_keeps_a_timed_out_command_outstanding() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let first_controller = shared(
        UpgradeController::new(
            db.clone(),
            hub.stream.manager().clone(),
            DESIRED,
            Arc::new(boss_spoke_upgrade::StaticCatalog(Some(published_catalog()))),
        )
        .with_upgrade_timeout(Duration::ZERO),
    );
    tick_offer(&first_controller, &mut first).await;
    tick(&first_controller).await;

    let mut next_catalog = published_catalog();
    next_catalog.release_id = "1.0.10".into();
    let next = shared(UpgradeController::new(
        db.clone(),
        hub.stream.manager().clone(),
        "1.0.10",
        Arc::new(boss_spoke_upgrade::StaticCatalog(Some(next_catalog))),
    ));
    tick(&next).await;
    second.expect_no_command().await;
    let outstanding = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert_eq!(outstanding.in_flight_host_id.as_deref(), Some("edge-a"));
    assert_eq!(
        outstanding.in_flight_command_id.as_deref(),
        Some("upgrade-edge-a-1.0.9")
    );
    assert!(outstanding.halted);
    assert_eq!(outstanding.halt_host_id.as_deref(), Some("edge-a"));
    assert_eq!(
        outstanding.halt_reason.as_deref(),
        Some("previous upgrade upgrade-edge-a-1.0.9 unresolved")
    );

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn desired_release_change_keeps_an_uncertain_command_outstanding() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let first_controller = shared(
        UpgradeController::new(
            db.clone(),
            hub.stream.manager().clone(),
            DESIRED,
            Arc::new(boss_spoke_upgrade::StaticCatalog(Some(published_catalog()))),
        )
        .with_command_deadline(Duration::from_millis(80)),
    );
    let ticking = {
        let controller = Arc::clone(&first_controller);
        tokio::spawn(async move { controller.tick().await })
    };
    let command = first.take_command().await;
    ticking.await.expect("join").expect("tick");
    assert_eq!(command.command_id, upgrade_command_id("edge-a", DESIRED));

    let mut next_catalog = published_catalog();
    next_catalog.release_id = "1.0.10".into();
    let next = shared(
        UpgradeController::new(
            db.clone(),
            hub.stream.manager().clone(),
            "1.0.10",
            Arc::new(boss_spoke_upgrade::StaticCatalog(Some(next_catalog))),
        )
        .with_command_deadline(Duration::from_millis(80)),
    );
    tick(&next).await;
    second.expect_no_command().await;
    let outstanding = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert_eq!(outstanding.in_flight_host_id.as_deref(), Some("edge-a"));
    assert_eq!(
        outstanding.in_flight_command_id.as_deref(),
        Some("upgrade-edge-a-1.0.9")
    );
    assert!(outstanding.halted);
    assert_eq!(outstanding.halt_host_id.as_deref(), Some("edge-a"));
    assert_eq!(
        outstanding.halt_reason.as_deref(),
        Some("previous upgrade upgrade-edge-a-1.0.9 unresolved")
    );

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn halted_rollout_does_not_reset_halt_at_on_later_ticks() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    tick_offer_rejected(&controller, &mut first, "upgrade_busy").await;
    let halted = db.spoke_upgrade_rollout().unwrap().unwrap();
    let halt_at = halted.halt_at.clone();
    let view = db.host_upgrade_view("edge-a").unwrap().unwrap();
    let last_upgrade_at = view.last_upgrade_at.clone();
    assert!(halt_at.is_some());
    tokio::time::sleep(Duration::from_secs(2)).await;
    tick(&controller).await;
    tick(&controller).await;
    let again = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert_eq!(again.halt_at, halt_at);
    let view_again = db.host_upgrade_view("edge-a").unwrap().unwrap();
    assert_eq!(view_again.last_upgrade_at, last_upgrade_at);
    second.expect_no_command().await;

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hub_refusal_survives_later_capacity_reports_and_ticks() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    tick_offer_rejected(&controller, &mut first, "upgrade_busy").await;
    let view = db.host_upgrade_view("edge-a").unwrap().unwrap();
    let stamped = view.last_upgrade_at.clone();
    assert_eq!(view.last_upgrade_result.as_deref(), Some("refused"));
    first.report_inventory("1.0.1", "ok", "", "upgrade-edge-a-1.0.1").await;
    tick(&controller).await;
    tick(&controller).await;
    let after = db.host_upgrade_view("edge-a").unwrap().unwrap();
    assert_eq!(after.last_upgrade_result.as_deref(), Some("refused"));
    assert_eq!(after.last_upgrade_error.as_deref(), Some("upgrade_busy"));
    assert_eq!(after.last_upgrade_at, stamped);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn outbox_read_errors_do_not_clear_in_flight_or_offer_another_host() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let reads = Arc::new(ReadFaults::default());
    let hub = start_with_outbox(db.clone(), fx.credentials.clone(), Some(Arc::clone(&reads))).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(rollout(&db, &hub, Some(published_catalog())));
    tick_offer(&controller, &mut first).await;
    reads.fail();
    drop(first);
    tokio::time::sleep(Duration::from_millis(800)).await;
    tick(&controller).await;
    second.expect_no_command().await;
    let outstanding = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert_eq!(outstanding.in_flight_host_id.as_deref(), Some("edge-a"));
    assert!(!outstanding.halted);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_persisted_progress_does_not_count_as_connected_time() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(
        UpgradeController::new(
            db.clone(),
            hub.stream.manager().clone(),
            DESIRED,
            Arc::new(boss_spoke_upgrade::StaticCatalog(Some(published_catalog()))),
        )
        .with_upgrade_timeout(Duration::from_secs(30)),
    );
    let command = tick_offer(&controller, &mut first).await;
    let mut row = db.spoke_upgrade_rollout().unwrap().unwrap();
    row.in_flight_last_progress_at = Some("1".into());
    row.in_flight_connected_secs = 0;
    db.save_spoke_upgrade_rollout(&row).unwrap();

    let restarted = shared(
        UpgradeController::new(
            db.clone(),
            hub.stream.manager().clone(),
            DESIRED,
            Arc::new(boss_spoke_upgrade::StaticCatalog(Some(published_catalog()))),
        )
        .with_upgrade_timeout(Duration::from_secs(30)),
    );
    tick(&restarted).await;
    let after = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(!after.halted);
    assert!(after.in_flight_connected_secs < 30);
    first.report_inventory(DESIRED, "ok", "", &command.command_id).await;
    tick(&restarted).await;
    let settled = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(!settled.halted);
    assert_ne!(
        settled.in_flight_command_id.as_deref(),
        Some(command.command_id.as_str())
    );

    hub.stop().await;
}

fn controller_with(
    db: &WorkDb,
    hub: &Running,
    desired: &str,
    catalog: boss_spoke_upgrade::SpokePackageCatalog,
) -> UpgradeController {
    UpgradeController::new(
        db.clone(),
        hub.stream.manager().clone(),
        desired,
        Arc::new(boss_spoke_upgrade::StaticCatalog(Some(catalog))),
    )
}

fn next_catalog() -> boss_spoke_upgrade::SpokePackageCatalog {
    let mut catalog = published_catalog();
    catalog.release_id = "1.0.10".into();
    catalog
}

/// Offer to `first`, then durably commit an OUTCOME_UNCERTAIN result for it.
async fn offer_with_durable_uncertain_result(controller: &Arc<UpgradeController>, first: &mut Peer) -> v1::Command {
    let ticking = {
        let controller = Arc::clone(controller);
        tokio::spawn(async move { controller.tick().await })
    };
    let command = first.take_command().await;
    ticking.await.expect("join").expect("tick");
    first
        .send_record(result_record(
            first.next_sequence,
            &command.command_id,
            ResultCode::OutcomeUncertain,
            "",
        ))
        .await;
    first.next_sequence += 1;
    command
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_uncertain_result_is_settled_by_matching_helper_success() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(
        controller_with(&db, &hub, DESIRED, published_catalog()).with_command_deadline(Duration::from_millis(80)),
    );
    let command = offer_with_durable_uncertain_result(&controller, &mut first).await;
    tick(&controller).await;
    second.expect_no_command().await;
    assert_eq!(
        db.spoke_upgrade_rollout()
            .unwrap()
            .unwrap()
            .in_flight_host_id
            .as_deref(),
        Some("edge-a")
    );

    first.report_inventory(DESIRED, "ok", "", &command.command_id).await;
    let offered = tick_offer(&controller, &mut second).await;
    assert_eq!(offered.command_id, upgrade_command_id("edge-b", DESIRED));
    assert!(!db.spoke_upgrade_rollout().unwrap().unwrap().halted);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_uncertain_result_without_helper_evidence_times_out() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    first.report_inventory("1.0.1", "", "", "").await;

    let controller = shared(
        controller_with(&db, &hub, DESIRED, published_catalog())
            .with_command_deadline(Duration::from_millis(80))
            .with_upgrade_timeout(Duration::ZERO),
    );
    offer_with_durable_uncertain_result(&controller, &mut first).await;
    tick(&controller).await;
    let row = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(row.halted);
    assert_eq!(row.halt_reason.as_deref(), Some("upgrade_timeout"));
    let view = db.host_upgrade_view("edge-a").unwrap().unwrap();
    assert_eq!(view.state, HostUpgradeState::Failed);

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_uncertain_result_holds_the_gate_across_a_release_change() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let mut first = enroll_and_connect(&hub, &fx.credentials, "edge-a").await;
    let mut second = enroll_and_connect(&hub, &fx.credentials, "edge-b").await;
    first.report_inventory("1.0.1", "", "", "").await;
    second.report_inventory("1.0.1", "", "", "").await;

    let initial = shared(
        controller_with(&db, &hub, DESIRED, published_catalog()).with_command_deadline(Duration::from_millis(80)),
    );
    let command = offer_with_durable_uncertain_result(&initial, &mut first).await;

    let next =
        shared(controller_with(&db, &hub, "1.0.10", next_catalog()).with_command_deadline(Duration::from_millis(80)));
    tick(&next).await;
    second.expect_no_command().await;
    let held = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert_eq!(held.in_flight_host_id.as_deref(), Some("edge-a"));
    assert!(held.halted, "the stall must be surfaced");
    assert_eq!(
        held.halt_reason.as_deref(),
        Some("previous upgrade upgrade-edge-a-1.0.9 unresolved")
    );
    assert_eq!(
        db.host_upgrade_view("edge-a").unwrap().unwrap().state,
        HostUpgradeState::Failed
    );
    tick(&next).await;
    second.expect_no_command().await;

    first.report_inventory(DESIRED, "ok", "", &command.command_id).await;
    tick(&next).await;
    let settled = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(!settled.halted);
    assert_ne!(
        settled.in_flight_command_id.as_deref(),
        Some(command.command_id.as_str())
    );

    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn suspension_and_reconnect_are_not_charged_and_success_wins() {
    let fx = Credentials::new();
    let (_dir, db) = work_db();
    let hub = start(db.clone(), fx.credentials.clone()).await;
    let identity = enroll_identity(&hub, &fx.credentials, "edge-a").await;
    let mut first = connect(dial(&hub, identity.clone()).await, hello("edge-a")).await;
    first.report_inventory("1.0.1", "", "", "").await;

    let now = Arc::new(AtomicU64::new(0));
    let clock = Arc::clone(&now);
    let controller = shared(
        controller_with(&db, &hub, DESIRED, published_catalog())
            .with_upgrade_timeout(Duration::from_secs(30))
            .with_clock(move || Duration::from_secs(clock.load(Ordering::SeqCst))),
    );
    let command = tick_offer(&controller, &mut first).await;
    tick(&controller).await;
    let charged = |db: &WorkDb| db.spoke_upgrade_rollout().unwrap().unwrap().in_flight_connected_secs;
    assert_eq!(charged(&db), 0);

    now.store(10, Ordering::SeqCst);
    tick(&controller).await;
    assert_eq!(charged(&db), 10);

    // The hub was suspended for a long time between ticks: not charged.
    now.store(1_000, Ordering::SeqCst);
    tick(&controller).await;
    assert_eq!(charged(&db), 10);
    assert!(!db.spoke_upgrade_rollout().unwrap().unwrap().halted);

    // A reconnect between two ticks starts a new baseline.
    let last_record = first.next_sequence - 1;
    drop(first);
    let mut reconnected = connect(
        dial(&hub, identity).await,
        v1::Hello {
            last_record,
            ..hello("edge-a")
        },
    )
    .await;
    reconnected.next_sequence = last_record + 1;
    // The hub replays the unacknowledged upgrade command on the new stream.
    assert_eq!(reconnected.take_command().await.command_id, command.command_id);
    now.store(1_010, Ordering::SeqCst);
    tick(&controller).await;
    assert_eq!(charged(&db), 10);
    now.store(1_020, Ordering::SeqCst);
    tick(&controller).await;
    assert_eq!(charged(&db), 20);

    // Matching success wins over the timeout even once the budget is spent.
    now.store(1_030, Ordering::SeqCst);
    reconnected
        .report_inventory(DESIRED, "ok", "", &command.command_id)
        .await;
    tick(&controller).await;
    let settled = db.spoke_upgrade_rollout().unwrap().unwrap();
    assert!(!settled.halted);
    assert!(settled.in_flight_host_id.is_none());

    hub.stop().await;
}
