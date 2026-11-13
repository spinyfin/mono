use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use boss_host_enrollment::{Clock, HostCredentials, Policy, pem_encode};
use boss_protocol_spoke::{
    negotiate::Contracts,
    v1::{self, CommandResult, Hello, Record, ResultCode, Stamp, command, record, spoke_frame},
};
use boss_spoke_hub::HubStream;
use boss_spoke_stream::Config;
use boss_spoke_upgrade::{SpokePackage, SpokePackageCatalog, StaticCatalog, UpgradeController};
use boss_work_db::WorkDb;
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tonic::Streaming;
use tonic::transport::{Certificate, Identity};

pub const STEP: Duration = Duration::from_secs(10);
pub const NO_COMMAND: Duration = Duration::from_millis(400);
pub const DESIRED: &str = "1.0.9";
pub const DIGEST_HEX: &str = "abababababababababababababababababababababababababababababababab";
pub const MANIFEST_URL: &str =
    "https://github.com/spinyfin/mono/releases/download/boss-spoke-v1.0.9/boss-spoke-linux-amd64.manifest.json";

pub struct ManualClock(std::sync::Mutex<SystemTime>);

impl ManualClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(std::sync::Mutex::new(SystemTime::now())))
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

pub struct Credentials {
    pub credentials: Arc<HostCredentials>,
    _clock: Arc<ManualClock>,
    _dir: TempDir,
}

impl Credentials {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let clock = ManualClock::new();
        let clock_trait: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
        let credentials =
            Arc::new(HostCredentials::open(dir.path(), Policy::default(), clock_trait).expect("open credentials"));
        Self {
            credentials,
            _clock: clock,
            _dir: dir,
        }
    }
}

pub fn work_db() -> (TempDir, WorkDb) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = WorkDb::open(dir.path().join("work.db")).expect("open work db");
    (dir, db)
}

pub fn hello(host_id: &str) -> Hello {
    Hello {
        host_id: host_id.into(),
        journal_id: format!("journal-{host_id}"),
        boot_id: "boot".into(),
        min_epoch: 1,
        max_epoch: 1,
        features: Contracts::first_release().features,
        instance_nonce: vec![1; 16],
        first_retained: 1,
        ..Default::default()
    }
}

pub fn stamp() -> Stamp {
    Stamp {
        boot_id: "boot".into(),
        wall_unix_ms: 1,
        ..Default::default()
    }
}

pub fn inventory_capacity_on(
    sequence: u64,
    os: &str,
    arch: &str,
    release_id: &str,
    last_result: &str,
    last_error: &str,
    command_id: &str,
) -> Record {
    Record {
        sequence,
        observed: Some(stamp()),
        run: None,
        body: Some(record::Body::Capacity(v1::Capacity {
            generation: sequence,
            total_slots: 1,
            inventory: Some(v1::Inventory {
                os: os.into(),
                arch: arch.into(),
                release_id: release_id.into(),
                last_upgrade_command_id: command_id.into(),
                last_upgrade_result: last_result.into(),
                last_upgrade_error: last_error.into(),
                ..Default::default()
            }),
            ..Default::default()
        })),
    }
}

pub fn result_record(sequence: u64, command_id: &str, code: ResultCode, reason: &str) -> Record {
    Record {
        sequence,
        observed: Some(stamp()),
        run: None,
        body: Some(record::Body::Result(CommandResult {
            command_id: command_id.into(),
            code: code.into(),
            reason: reason.into(),
            ..Default::default()
        })),
    }
}

pub fn published_catalog() -> SpokePackageCatalog {
    let mut packages = std::collections::BTreeMap::new();
    packages.insert(
        "linux-amd64".into(),
        SpokePackage {
            manifest_url: MANIFEST_URL.into(),
            manifest_sha256: DIGEST_HEX.into(),
        },
    );
    SpokePackageCatalog {
        release_id: DESIRED.into(),
        packages,
    }
}

pub fn controller(
    db: WorkDb,
    streams: boss_spoke_stream::StreamManager,
    catalog: Option<SpokePackageCatalog>,
) -> UpgradeController {
    UpgradeController::new(db, streams, DESIRED, Arc::new(StaticCatalog(catalog)))
}

pub fn hub_config() -> Config {
    Config::builder()
        .hub_id("hub")
        .authority_generation("authority")
        .build_fingerprint("test")
        .max_hosts(4)
        .max_pending(4)
        .hello_timeout(Duration::from_millis(400))
        .pulse_interval(Duration::from_secs(60))
        .pulse_timeout(Duration::from_millis(500))
        .build()
}

pub fn hub_stream(db: WorkDb, credentials: Arc<HostCredentials>) -> HubStream {
    HubStream::new(hub_config(), credentials, db)
}

pub struct ServerPki {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

impl ServerPki {
    pub fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name.push(DnType::CommonName, "test server CA");
        Self {
            issuer: CertifiedIssuer::self_signed(params, KeyPair::generate().expect("ca")).expect("ca"),
        }
    }

    pub fn trusted(&self) -> Certificate {
        Certificate::from_pem(self.issuer.pem())
    }

    pub fn identity(&self) -> Identity {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![
            SanType::DnsName(Ia5String::try_from("localhost").expect("dns")),
            SanType::IpAddress("127.0.0.1".parse().expect("ip")),
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let cert = params.signed_by(&key, &self.issuer).expect("server cert");
        Identity::from_pem(cert.pem(), key.serialize_pem())
    }
}

pub struct Spoke {
    pub key: KeyPair,
}

impl Spoke {
    pub fn new() -> Self {
        Self {
            key: KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("key"),
        }
    }

    pub fn csr(&self) -> Vec<u8> {
        CertificateParams::default()
            .serialize_request(&self.key)
            .expect("csr")
            .der()
            .to_vec()
    }

    pub fn identity(&self, cert: &v1::Certificate) -> Identity {
        Identity::from_pem(pem_encode("CERTIFICATE", &cert.leaf_der), self.key.serialize_pem())
    }
}

pub struct Peer {
    pub tx: mpsc::Sender<v1::SpokeFrame>,
    pub rx: Streaming<v1::HubFrame>,
    pub next_sequence: u64,
}

impl Peer {
    pub async fn next(&mut self) -> Option<v1::hub_frame::Body> {
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

    pub async fn send_record(&mut self, record: Record) {
        self.tx
            .send(v1::SpokeFrame {
                body: Some(spoke_frame::Body::Record(record)),
            })
            .await
            .unwrap();
        match self.next().await {
            Some(v1::hub_frame::Body::RecordReceipt(_)) => {}
            other => panic!("expected receipt, got {other:?}"),
        }
    }

    pub async fn report_inventory(&mut self, release_id: &str, last_result: &str, last_error: &str, command_id: &str) {
        self.report_inventory_on("linux", "x86_64", release_id, last_result, last_error, command_id)
            .await;
    }

    pub async fn report_inventory_on(
        &mut self,
        os: &str,
        arch: &str,
        release_id: &str,
        last_result: &str,
        last_error: &str,
        command_id: &str,
    ) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.send_record(inventory_capacity_on(
            sequence,
            os,
            arch,
            release_id,
            last_result,
            last_error,
            command_id,
        ))
        .await;
    }

    pub async fn take_command(&mut self) -> v1::Command {
        match self.next().await {
            Some(v1::hub_frame::Body::Command(command)) => command,
            other => panic!("expected command, got {other:?}"),
        }
    }

    pub async fn accept_command(&mut self, code: ResultCode, reason: &str) -> v1::Command {
        let command = match self.next().await {
            Some(v1::hub_frame::Body::Command(command)) => command,
            other => panic!("expected command, got {other:?}"),
        };
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.send_record(result_record(sequence, &command.command_id, code, reason))
            .await;
        command
    }

    pub async fn expect_no_command(&mut self) {
        match tokio::time::timeout(NO_COMMAND, self.next()).await {
            Err(_) => {}
            Ok(Some(v1::hub_frame::Body::Command(command))) => {
                panic!("unexpected command {}", command.command_id)
            }
            Ok(other) => panic!("unexpected frame {other:?}"),
        }
    }
}

pub fn upgrade_of(command: &v1::Command) -> &v1::Upgrade {
    match &command.body {
        Some(command::Body::Upgrade(upgrade)) => upgrade,
        other => panic!("expected Upgrade, got {other:?}"),
    }
}
