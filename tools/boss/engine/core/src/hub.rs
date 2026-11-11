//! Hub listener configuration and production startup.
//!
//! The hub stream is inert until listen address, credential directory and
//! TLS identity are all set. Fixtures always derive credentials and accept
//! only an ephemeral loopback bind with TLS files inside the derived directory.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use boss_host_enrollment::{HostCredentials, Policy, SystemClock};
pub use boss_spoke_hub::HubStream;
use tokio::net::TcpListener;

use crate::work::WorkDb;
use tonic::transport::Identity;

/// Environment override for the hub gRPC listen address (`IP:port`).
pub const LISTEN_ENV: &str = "BOSS_HUB_LISTEN";
/// Environment override for the host-credential directory (`dir/ca` and
/// `dir/credentials.db`).
pub const CREDENTIALS_DIR_ENV: &str = "BOSS_HUB_CREDENTIALS_DIR";
/// Environment override for the hub server TLS certificate PEM.
pub const TLS_CERT_ENV: &str = "BOSS_HUB_TLS_CERT";
/// Environment override for the hub server TLS private key PEM.
pub const TLS_KEY_ENV: &str = "BOSS_HUB_TLS_KEY";
/// Environment override for the hub identity string carried in `Welcome`.
pub const HUB_ID_ENV: &str = "BOSS_HUB_ID";
/// Environment override for the authority generation carried in `Welcome`.
pub const AUTHORITY_GENERATION_ENV: &str = "BOSS_HUB_AUTHORITY_GENERATION";

/// Default [`HubConfig::hub_id`] when `BOSS_HUB_ID` is unset.
pub const DEFAULT_HUB_ID: &str = "hub";
/// Default [`HubConfig::authority_generation`] when unset.
pub const DEFAULT_HUB_AUTHORITY_GENERATION: &str = "1";

/// Configuration for the hub stream listener.
///
/// All four of listen, credentials directory, TLS cert and TLS key must be
/// set before the engine binds a socket. The default (everything unset) is
/// inert: no listener is bound and dispatch stays on SSH.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct HubConfig {
    pub listen: Option<String>,
    pub credentials_dir: Option<PathBuf>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    #[builder(default = DEFAULT_HUB_ID.to_owned())]
    pub hub_id: String,
    #[builder(default = DEFAULT_HUB_AUTHORITY_GENERATION.to_owned())]
    pub authority_generation: String,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            listen: None,
            credentials_dir: None,
            tls_cert: None,
            tls_key: None,
            hub_id: DEFAULT_HUB_ID.to_owned(),
            authority_generation: DEFAULT_HUB_AUTHORITY_GENERATION.to_owned(),
        }
    }
}

/// Fully specified listener bind plan. Constructed only when every required
/// field is present and well-formed.
#[derive(Debug, Clone)]
pub struct HubListenerPlan {
    pub listen: SocketAddr,
    pub credentials_dir: PathBuf,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
}

/// A bound hub listener and its shared [`HubStream`] handle. The caller owns
/// serving the incoming socket (typically on the engine task scope).
pub struct BoundHub {
    pub stream: HubStream,
    pub local_addr: SocketAddr,
    incoming: TcpListener,
    identity: Identity,
}

impl BoundHub {
    /// Serve `Enroll`/`Renew` and `Connect` until the listener is dropped.
    pub async fn serve(self) -> Result<(), tonic::transport::Error> {
        self.stream.serve(self.incoming, self.identity).await
    }
}

impl HubConfig {
    /// Load hub configuration from an explicit env lookup.
    pub fn load_from(lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Self> {
        let nonempty = |name: &str| -> Option<String> {
            lookup(name)
                .map(|val| val.to_string_lossy().into_owned())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let path = |name: &str| {
            lookup(name)
                .filter(|value| !value.to_string_lossy().trim().is_empty())
                .map(PathBuf::from)
        };
        let config = Self {
            listen: nonempty(LISTEN_ENV),
            credentials_dir: path(CREDENTIALS_DIR_ENV),
            tls_cert: path(TLS_CERT_ENV),
            tls_key: path(TLS_KEY_ENV),
            hub_id: nonempty(HUB_ID_ENV).unwrap_or_else(|| DEFAULT_HUB_ID.to_owned()),
            authority_generation: nonempty(AUTHORITY_GENERATION_ENV)
                .unwrap_or_else(|| DEFAULT_HUB_AUTHORITY_GENERATION.to_owned()),
        };
        let _ = config.listener_plan()?;
        Ok(config)
    }

    /// Fixtures use derived credentials, TLS files beneath that directory,
    /// and an ephemeral loopback port. Inherited identity is never opened.
    pub fn isolate_for_fixture(self, is_test_fixture: bool, derived_credentials_dir: Option<PathBuf>) -> Self {
        if !is_test_fixture {
            return self;
        }
        let HubConfig {
            listen,
            credentials_dir: _,
            tls_cert,
            tls_key,
            hub_id,
            authority_generation,
        } = self;
        let listen = listen.filter(|addr| fixture_safe_listen(addr));
        let tls_cert = tls_cert.filter(|path| {
            derived_credentials_dir
                .as_ref()
                .is_some_and(|root| fixture_owned_path(path, root))
        });
        let tls_key = tls_key.filter(|path| {
            derived_credentials_dir
                .as_ref()
                .is_some_and(|root| fixture_owned_path(path, root))
        });
        let credentials_dir = derived_credentials_dir;
        if listen.is_some() && tls_cert.is_some() && tls_key.is_some() && credentials_dir.is_some() {
            Self {
                listen,
                credentials_dir,
                tls_cert,
                tls_key,
                hub_id,
                authority_generation,
            }
        } else {
            Self {
                listen: None,
                credentials_dir: None,
                tls_cert: None,
                tls_key: None,
                hub_id,
                authority_generation,
            }
        }
    }

    /// `Ok(None)` when the listener is inert. `Err` when configuration sets
    /// some of the four required fields but not all of them, or when listen
    /// is not a socket address.
    pub fn listener_plan(&self) -> Result<Option<HubListenerPlan>> {
        let present = [
            self.listen.is_some(),
            self.credentials_dir.is_some(),
            self.tls_cert.is_some(),
            self.tls_key.is_some(),
        ];
        let any = present.iter().any(|set| *set);
        let all = present.iter().all(|set| *set);
        if !any {
            return Ok(None);
        }
        if !all {
            bail!(
                "{LISTEN_ENV}, {CREDENTIALS_DIR_ENV}, {TLS_CERT_ENV}, and {TLS_KEY_ENV} must all be set to start the hub listener"
            );
        }
        let listen = self
            .listen
            .as_deref()
            .unwrap()
            .parse::<SocketAddr>()
            .with_context(|| format!("{LISTEN_ENV} is not a socket address: {:?}", self.listen))?;
        Ok(Some(HubListenerPlan {
            listen,
            credentials_dir: self.credentials_dir.clone().unwrap(),
            tls_cert: self.tls_cert.clone().unwrap(),
            tls_key: self.tls_key.clone().unwrap(),
        }))
    }
}

/// Bind the hub listener when [`HubConfig::listener_plan`] is ready. Does not
/// spawn the serve task: the caller holds [`BoundHub::stream`] as the shared
/// manager handle and serves on its own task scope.
pub async fn bind_hub_listener(config: &HubConfig, db: WorkDb) -> Result<Option<BoundHub>> {
    let Some(plan) = config.listener_plan()? else {
        return Ok(None);
    };
    if config.hub_id.trim().is_empty() {
        bail!("{HUB_ID_ENV} must not be empty");
    }
    if config.authority_generation.trim().is_empty() {
        bail!("{AUTHORITY_GENERATION_ENV} must not be empty");
    }
    let credentials = Arc::new(
        HostCredentials::open(&plan.credentials_dir, Policy::default(), Arc::new(SystemClock))
            .with_context(|| format!("opening hub credentials at {}", plan.credentials_dir.display()))?,
    );
    let identity = load_server_identity(&plan.tls_cert, &plan.tls_key)?;
    tonic::transport::Server::builder()
        .tls_config(boss_spoke_hub::server_tls(identity.clone(), &credentials)?)
        .context("validating hub TLS identity")?;
    let incoming = TcpListener::bind(plan.listen)
        .await
        .with_context(|| format!("binding hub listener on {}", plan.listen))?;
    let local_addr = incoming.local_addr().context("hub listener local address")?;
    let stream = HubStream::with_identity(
        config.hub_id.clone(),
        config.authority_generation.clone(),
        crate::build_info::build_identity(),
        credentials,
        db,
    );
    tracing::info!(addr = %local_addr, hub_id = %config.hub_id, "hub listener bound");
    Ok(Some(BoundHub {
        stream,
        local_addr,
        incoming,
        identity,
    }))
}

fn load_server_identity(cert: &Path, key: &Path) -> Result<Identity> {
    let cert_pem = std::fs::read(cert).with_context(|| format!("reading hub TLS certificate {}", cert.display()))?;
    let key_pem = std::fs::read(key).with_context(|| format!("reading hub TLS key {}", key.display()))?;
    Ok(Identity::from_pem(cert_pem, key_pem))
}

fn fixture_owned_path(path: &Path, root: &Path) -> bool {
    match (path.canonicalize(), root.canonicalize()) {
        (Ok(path), Ok(root)) => path.starts_with(&root) && path != root,
        _ => false,
    }
}

fn fixture_safe_listen(listen: &str) -> bool {
    listen
        .parse::<SocketAddr>()
        .is_ok_and(|addr| addr.ip().is_loopback() && addr.port() == 0)
}

#[cfg(test)]
mod tests {
    /// Production-shaped paths for fixture regression tests.
    #[derive(Debug, Clone, Default)]
    pub struct HubProductionPaths {
        pub credentials_dir: Option<PathBuf>,
        pub tls_cert: Option<PathBuf>,
        pub tls_key: Option<PathBuf>,
    }

    impl HubProductionPaths {
        /// Resolve production locations as if the state root were `root`.
        pub fn under_state_root(root: Option<&Path>) -> Self {
            Self {
                credentials_dir: root.map(|root| root.join(CREDENTIALS_DIRNAME)),
                tls_cert: root.map(|root| root.join(TLS_CERT_FILENAME)),
                tls_key: root.map(|root| root.join(TLS_KEY_FILENAME)),
            }
        }
    }

    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use boss_host_enrollment::pem_encode;
    use boss_protocol_spoke::negotiate::Contracts;
    use boss_protocol_spoke::v1::enrollment_client::EnrollmentClient;
    use boss_protocol_spoke::v1::spoke_hub_client::SpokeHubClient;
    use boss_protocol_spoke::v1::{self, EnrollRequest, spoke_frame};

    use rcgen::string::Ia5String;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType,
    };
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Code;
    use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

    const CREDENTIALS_DIRNAME: &str = "hub-credentials";
    const TLS_CERT_FILENAME: &str = "hub-tls.crt";
    const TLS_KEY_FILENAME: &str = "hub-tls.key";

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(*v))
    }

    #[test]
    fn empty_path_variables_are_unset() {
        let config =
            HubConfig::load_from(env(&[(TLS_CERT_ENV, ""), (TLS_KEY_ENV, ""), (CREDENTIALS_DIR_ENV, "")])).unwrap();
        assert!(config.listener_plan().unwrap().is_none());
    }

    #[test]
    fn custom_inherited_identity_is_never_used_by_a_fixture() {
        let config = HubConfig::builder()
            .listen("127.0.0.1:8443")
            .credentials_dir(PathBuf::from("/custom/identity"))
            .tls_cert(PathBuf::from("/custom/server.pem"))
            .tls_key(PathBuf::from("/custom/key.pem"))
            .build();
        let isolated = config.isolate_for_fixture(true, Some(PathBuf::from("/tmp/fixture-identity")));
        assert!(isolated.listener_plan().unwrap().is_none());
    }

    #[test]
    fn fixture_guards_are_independent_and_reject_escaping_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let derived = root.path().join("credentials");
        std::fs::create_dir(&derived).unwrap();
        let inside = derived.join("identity");
        let outside = root.path().join("outside");
        std::fs::write(&inside, "identity").unwrap();
        std::fs::write(&outside, "identity").unwrap();
        let link = derived.join("symlink");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        for (listen, identity, enabled) in [
            ("127.0.0.1:0", &inside, true),
            ("127.0.0.1:0", &outside, false),
            ("127.0.0.1:8443", &inside, false),
            ("0.0.0.0:0", &inside, false),
            ("127.0.0.1:0", &link, false),
        ] {
            let config = HubConfig::builder()
                .listen(listen)
                .credentials_dir(derived.clone())
                .tls_cert(identity.clone())
                .tls_key(identity.clone())
                .build()
                .isolate_for_fixture(true, Some(derived.clone()));
            assert_eq!(
                config.listener_plan().unwrap().is_some(),
                enabled,
                "{listen} {identity:?}"
            );
        }
    }

    #[tokio::test]
    async fn invalid_tls_key_fails_before_listener_is_ready() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let root = tempfile::tempdir().unwrap();
        let cert = root.path().join("cert.pem");
        let key = root.path().join("key.pem");
        ServerPki::new().write_identity(&cert, &key);
        std::fs::write(&key, "not a PEM private key").unwrap();
        let config = HubConfig::builder()
            .listen("127.0.0.1:0")
            .credentials_dir(root.path().join("credentials"))
            .tls_cert(cert)
            .tls_key(key)
            .build();
        let db = WorkDb::open(root.path().join("state.db")).unwrap();
        let error = bind_hub_listener(&config, db)
            .await
            .err()
            .expect("invalid key rejected at bind");
        assert!(
            format!("{error:#}").contains("validating hub TLS identity"),
            "{error:#}"
        );
    }

    #[test]
    fn hub_config_is_inert_when_unset() {
        let config = HubConfig::load_from(|_| None).unwrap();
        assert!(config.listener_plan().unwrap().is_none());
        assert_eq!(config.hub_id, DEFAULT_HUB_ID);
        assert_eq!(config.authority_generation, DEFAULT_HUB_AUTHORITY_GENERATION);
    }

    #[test]
    fn hub_config_reads_env_and_requires_all_listener_fields() {
        let err = HubConfig::load_from(env(&[(LISTEN_ENV, "127.0.0.1:8443")])).unwrap_err();
        assert!(
            err.to_string().contains(CREDENTIALS_DIR_ENV),
            "partial config error was: {err:#}"
        );

        let config = HubConfig::load_from(env(&[
            (LISTEN_ENV, "127.0.0.1:8443"),
            (CREDENTIALS_DIR_ENV, "/tmp/hub-credentials"),
            (TLS_CERT_ENV, "/tmp/hub.crt"),
            (TLS_KEY_ENV, "/tmp/hub.key"),
            (HUB_ID_ENV, "test-hub"),
            (AUTHORITY_GENERATION_ENV, "gen-9"),
        ]))
        .unwrap();
        let plan = config.listener_plan().unwrap().unwrap();
        assert_eq!(plan.listen, "127.0.0.1:8443".parse::<SocketAddr>().unwrap());
        assert_eq!(plan.credentials_dir, PathBuf::from("/tmp/hub-credentials"));
        assert_eq!(config.hub_id, "test-hub");
        assert_eq!(config.authority_generation, "gen-9");
    }

    #[test]
    fn fixture_isolation_skips_production_identity_and_non_loopback_listen() {
        let production_root = PathBuf::from("/Users/someone/Library/Application Support/Boss");
        let production = HubProductionPaths::under_state_root(Some(&production_root));
        let derived = PathBuf::from("/tmp/boss-test-uuid.hub-credentials");
        let configured = HubConfig {
            listen: Some("0.0.0.0:8443".into()),
            credentials_dir: production.credentials_dir.clone(),
            tls_cert: production.tls_cert.clone(),
            tls_key: production.tls_key.clone(),
            hub_id: "hub".into(),
            authority_generation: "1".into(),
        };
        let isolated = configured.isolate_for_fixture(true, Some(derived.clone()));
        assert!(isolated.listener_plan().unwrap().is_none());
        assert!(isolated.listen.is_none());
        assert!(isolated.credentials_dir.is_none());
    }

    #[test]
    fn fixture_isolation_derives_credentials_when_listen_and_tls_are_private() {
        let production_root = PathBuf::from("/Users/someone/Library/Application Support/Boss");
        let production = HubProductionPaths::under_state_root(Some(&production_root));
        let root = tempfile::tempdir().unwrap();
        let derived = root.path().join("credentials");
        std::fs::create_dir(&derived).unwrap();
        let cert = derived.join("fixture.crt");
        let key = derived.join("fixture.key");
        std::fs::write(&cert, "cert").unwrap();
        std::fs::write(&key, "key").unwrap();
        let configured = HubConfig {
            listen: Some("127.0.0.1:0".into()),
            credentials_dir: production.credentials_dir.clone(),
            tls_cert: Some(cert.clone()),
            tls_key: Some(key),
            hub_id: "hub".into(),
            authority_generation: "1".into(),
        };
        let isolated = configured.isolate_for_fixture(true, Some(derived.clone()));
        let plan = isolated.listener_plan().unwrap().expect("fixture hub should start");
        assert!(plan.listen.ip().is_loopback());
        assert_eq!(plan.credentials_dir, derived);
        assert_eq!(plan.tls_cert, cert);
    }

    #[test]
    fn production_engine_keeps_configured_hub_paths() {
        let production =
            HubProductionPaths::under_state_root(Some(Path::new("/Users/someone/Library/Application Support/Boss")));
        let configured = HubConfig {
            listen: Some("0.0.0.0:8443".into()),
            credentials_dir: production.credentials_dir.clone(),
            tls_cert: production.tls_cert.clone(),
            tls_key: production.tls_key.clone(),
            hub_id: "hub".into(),
            authority_generation: "1".into(),
        };
        let kept = configured
            .clone()
            .isolate_for_fixture(false, Some(PathBuf::from("/tmp/unused")));
        assert_eq!(kept.listen, configured.listen);
        assert_eq!(kept.credentials_dir, configured.credentials_dir);
    }

    struct ServerPki {
        issuer: CertifiedIssuer<'static, KeyPair>,
    }

    impl ServerPki {
        fn new() -> Self {
            let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            params.distinguished_name.push(DnType::CommonName, "test server CA");
            Self {
                issuer: CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("ca"),
            }
        }

        fn write_identity(&self, cert_path: &Path, key_path: &Path) -> Certificate {
            let key = KeyPair::generate().expect("key");
            let mut params = CertificateParams::default();
            params.subject_alt_names = vec![
                SanType::DnsName(Ia5String::try_from("localhost").expect("dns")),
                SanType::IpAddress("127.0.0.1".parse().expect("ip")),
            ];
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            let cert = params.signed_by(&key, &self.issuer).expect("server cert");
            std::fs::write(cert_path, cert.pem()).expect("write cert");
            std::fs::write(key_path, key.serialize_pem()).expect("write key");
            Certificate::from_pem(self.issuer.pem())
        }
    }

    struct SpokeKey {
        key: KeyPair,
    }

    impl SpokeKey {
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

        fn identity(&self, cert: &v1::Certificate) -> Identity {
            Identity::from_pem(pem_encode("CERTIFICATE", &cert.leaf_der), self.key.serialize_pem())
        }
    }

    async fn dial(addr: SocketAddr, ca: Certificate, identity: Option<Identity>) -> Channel {
        let mut tls = ClientTlsConfig::new().ca_certificate(ca).domain_name("localhost");
        if let Some(identity) = identity {
            tls = tls.identity(identity);
        }
        Endpoint::from_shared(format!("https://127.0.0.1:{}", addr.port()))
            .unwrap()
            .tls_config(tls)
            .unwrap()
            .connect()
            .await
            .expect("dial")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bind_hub_listener_is_inert_when_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        let bound = bind_hub_listener(&HubConfig::default(), db).await.unwrap();
        assert!(bound.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn authenticated_listener_accepts_enroll_then_connect() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let root = tempfile::tempdir().unwrap();
        let credentials_dir = root.path().join("hub-credentials");
        std::fs::create_dir(&credentials_dir).unwrap();
        std::fs::set_permissions(&credentials_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cert_path = root.path().join("hub.crt");
        let key_path = root.path().join("hub.key");
        let pki = ServerPki::new();
        let server_ca = pki.write_identity(&cert_path, &key_path);
        let db = WorkDb::open(root.path().join("state.db")).unwrap();
        let config = HubConfig {
            listen: Some("127.0.0.1:0".into()),
            credentials_dir: Some(credentials_dir.clone()),
            tls_cert: Some(cert_path),
            tls_key: Some(key_path),
            hub_id: "hub".into(),
            authority_generation: "1".into(),
        };
        let bound = bind_hub_listener(&config, db)
            .await
            .unwrap()
            .expect("listener should start");
        let addr = bound.local_addr;
        let stream = bound.stream.clone();
        let credentials = stream.credentials().clone();
        let task = tokio::spawn(async move {
            let _ = bound.serve().await;
        });

        credentials.authorize_host("edge-1", "test").expect("authorize");
        let grant = credentials.issue_enrollment_token("edge-1", "job-1").expect("token");
        let spoke = SpokeKey::new();
        let mut anonymous = EnrollmentClient::new(dial(addr, server_ca.clone(), None).await);
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
        assert_eq!(cert.host_id, "edge-1");

        let mut unauthenticated = SpokeHubClient::new(dial(addr, server_ca.clone(), None).await);
        let (tx, rx) = mpsc::channel(4);
        tx.send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Hello(v1::Hello {
                host_id: "edge-1".into(),
                journal_id: "journal".into(),
                boot_id: "boot".into(),
                min_epoch: 1,
                max_epoch: 1,
                features: Contracts::first_release().features,
                instance_nonce: vec![1; 16],
                first_retained: 1,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let anonymous_connect = unauthenticated.connect(ReceiverStream::new(rx)).await;
        assert_eq!(
            anonymous_connect.expect_err("anonymous connect must fail").code(),
            Code::Unauthenticated
        );

        let mut authed = SpokeHubClient::new(dial(addr, server_ca, Some(spoke.identity(&cert))).await);
        let (tx, rx) = mpsc::channel(4);
        tx.send(v1::SpokeFrame {
            body: Some(spoke_frame::Body::Hello(v1::Hello {
                host_id: "edge-1".into(),
                journal_id: "journal".into(),
                boot_id: "boot".into(),
                min_epoch: 1,
                max_epoch: 1,
                features: Contracts::first_release().features,
                instance_nonce: vec![1; 16],
                first_retained: 1,
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let mut inbound = authed
            .connect(ReceiverStream::new(rx))
            .await
            .expect("authenticated connect")
            .into_inner();
        let welcome = tokio::time::timeout(Duration::from_secs(4), inbound.message())
            .await
            .expect("welcome bounded")
            .expect("stream")
            .expect("frame");
        match welcome.body {
            Some(v1::hub_frame::Body::Welcome(welcome)) => {
                assert_eq!(welcome.hub_id, "hub");
                assert_eq!(welcome.authority_generation, "1");
            }
            other => panic!("expected welcome, got {other:?}"),
        }

        drop(tx);
        task.abort();
        let _ = task.await;
    }
}
