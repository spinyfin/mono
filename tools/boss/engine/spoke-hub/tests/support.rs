use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use boss_host_enrollment::{Clock, HostCredentials, Policy};
use boss_protocol_spoke::{
    negotiate::{Contracts, Selection},
    v1::{self, Command, Hello, Record, Run, Stamp, command, record},
};
use boss_spoke_hub::{HubStream, WorkDbOutbox, WorkDbSink};
use boss_spoke_stream::{Config, store::Session};
use boss_work_db::{HostTransport, SpokeAssignment, SpokeRunKey, WorkDb};
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use tempfile::TempDir;
use tonic::transport::{Certificate, Identity};

pub const STEP: Duration = Duration::from_secs(10);

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

pub fn work_db_at(path: PathBuf) -> WorkDb {
    WorkDb::open(path).expect("open work db")
}

pub fn spoke_ready(db: &WorkDb, host: &str) {
    db.set_host_transport(host, HostTransport::SpokeReady)
        .expect("spoke ready");
}

pub fn run_key(assignment: &SpokeAssignment) -> SpokeRunKey {
    SpokeRunKey {
        execution_id: assignment.execution_id.clone(),
        assignment_generation: assignment.assignment_generation,
        run_id: assignment.run_id.clone(),
        spawn_token: assignment.spawn_token.clone(),
    }
}

pub fn proto_run(key: &SpokeRunKey) -> Run {
    Run {
        execution_id: key.execution_id.clone(),
        run_id: key.run_id.clone(),
        assignment_generation: key.assignment_generation,
        spawn_token: key.spawn_token.clone(),
    }
}

pub fn hello(host_id: &str) -> Hello {
    Hello {
        host_id: host_id.into(),
        journal_id: "journal".into(),
        boot_id: "boot".into(),
        min_epoch: 1,
        max_epoch: 1,
        features: Contracts::first_release().features,
        instance_nonce: vec![1; 16],
        first_retained: 1,
        ..Default::default()
    }
}

pub fn selection() -> Selection {
    Contracts::first_release().select(&hello("host-a")).unwrap()
}

pub fn session(host_id: &str, generation: u64) -> Session {
    Session {
        host_id: host_id.into(),
        journal_id: "journal".into(),
        generation,
        selection: selection(),
    }
}

pub fn stamp() -> Stamp {
    Stamp {
        boot_id: "boot".into(),
        wall_unix_ms: 1,
        ..Default::default()
    }
}

pub fn pane(sequence: u64, run: Option<Run>) -> Record {
    Record {
        sequence,
        observed: Some(stamp()),
        run,
        body: Some(record::Body::Pane(v1::PaneState {
            life: v1::PaneLife::Alive.into(),
            boot_id: "boot".into(),
            ..Default::default()
        })),
    }
}

pub fn capacity(sequence: u64) -> Record {
    Record {
        sequence,
        observed: Some(stamp()),
        run: None,
        body: Some(record::Body::Capacity(v1::Capacity {
            generation: 1,
            total_slots: 1,
            ..Default::default()
        })),
    }
}

pub fn stop_command(id: &str, run: Run) -> Command {
    Command {
        command_id: id.into(),
        run: Some(run),
        not_after_unix_ms: 4_102_444_800_000,
        body: Some(command::Body::Stop(v1::Stop { grace_ms: 1 })),
        ..Default::default()
    }
}

pub fn stores(db: WorkDb) -> (WorkDbSink, WorkDbOutbox) {
    (WorkDbSink::new(db.clone()), WorkDbOutbox::new(db))
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

/// Throwaway CA for the hub's server certificate, separate from the host CA.
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
            issuer: CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("ca"),
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
