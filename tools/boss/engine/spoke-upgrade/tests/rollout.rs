use std::net::SocketAddr;
use std::sync::Arc;

use boss_host_enrollment::HostCredentials;
use boss_protocol_spoke::v1::enrollment_client::EnrollmentClient;
use boss_protocol_spoke::v1::spoke_hub_client::SpokeHubClient;
use boss_protocol_spoke::v1::{self, EnrollRequest, ResultCode, spoke_frame};
use boss_spoke_hub::HubStream;
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
    connect(dial(hub, spoke.identity(&cert)).await, hello(host_id)).await
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
