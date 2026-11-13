//! Manual host enrollment against a real hub-side credential authority.
//!
//! The package is installed into a staging root with the real binaries
//! (`install_test` covers that step). Here a hub that serves the real `Enroll`
//! endpoint over TLS mints tokens, and `bootstrap` takes them through enroll,
//! service start and the as-user preflight. The as-user step is the real
//! `boss-spoke-package enroll --inline` process, so key generation, the TLS
//! call and the files it writes are all exercised. Only the service manager and
//! account creation are recorded rather than performed.

use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use boss_host_enrollment::{
    Denial, EnrollmentService, HostAuthorizer as _, HostCredentials, HostState, MANUAL_JOB_ID, Policy, ServiceLimits,
    SystemClock,
};
use boss_spoke_package::bootstrap::{self, Failure, Options};
use boss_spoke_package::install::{self, Account, Health, Host, Layout, Request, RunOutput};
use boss_spoke_package::manifest::{Platform, SigningKey};
use boss_spoke_package::{build, system};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use tonic::transport::{Identity, Server, ServerTlsConfig};

const URL_BASE: &str = "https://github.com/spinyfin/mono/releases/download/boss-spoke-v1.0.1";
const SEED: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
const HOST_ID: &str = "host-one";
const INSTANT: Health = Health {
    deadline: Duration::from_millis(50),
    poll: Duration::from_millis(1),
    stable: Duration::ZERO,
};

// ---------------------------------------------------------------------------
// A hub that answers `Enroll` over TLS, backed by the real authority.

struct Hub {
    credentials: Arc<HostCredentials>,
    /// The CA of the hub's TLS certificate: what a host installs as `ca.pem`.
    ca_pem: String,
    url: String,
    _runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

impl Hub {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self::serve(listener)
    }

    fn serve(listener: std::net::TcpListener) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let credentials =
            Arc::new(HostCredentials::open(dir.path(), Policy::default(), Arc::new(SystemClock)).unwrap());

        let mut ca = CertificateParams::default();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let issuer = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&key, &issuer).unwrap();
        let tls = ServerTlsConfig::new().identity(Identity::from_pem(cert.pem(), key.serialize_pem()));

        let url = format!("https://{}", listener.local_addr().unwrap());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let service = EnrollmentService::new(Arc::clone(&credentials), ServiceLimits::default()).into_server();
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(service)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            credentials,
            ca_pem: issuer.pem(),
            url,
            _runtime: runtime,
            _dir: dir,
        }
    }

    /// A URL on this machine that nothing listens on.
    fn closed_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("https://{}", listener.local_addr().unwrap())
    }

    fn mint(&self, host: &str) -> String {
        self.credentials
            .issue_manual_token(host, "test", false)
            .unwrap()
            .token
            .encode()
    }
}

// ---------------------------------------------------------------------------
// The host: the real as-user commands, a recorded service manager.

struct Fake {
    account: Account,
    registered: Cell<bool>,
    running: Cell<bool>,
    calls: RefCell<Vec<&'static str>>,
    probe: Probe,
}

/// What the as-user commands are made to report and what they were given.
struct Probe {
    /// What the as-user preflight reports instead of running: whether it
    /// passed, and its output. `None` runs the real checks.
    preflight: RefCell<Option<(bool, String)>>,
    /// Every argument and environment entry any as-user command was given.
    seen: RefCell<Vec<String>>,
}

impl Fake {
    fn new() -> Self {
        // SAFETY: geteuid and getegid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self {
            account: Account { uid, gid },
            registered: Cell::new(false),
            running: Cell::new(false),
            calls: RefCell::default(),
            probe: Probe {
                preflight: RefCell::new(Some((true, "PASS tmux: stubbed\n".to_owned()))),
                seen: RefCell::default(),
            },
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }
}

impl Host for Fake {
    fn lookup_user(&self, _: &str) -> Result<Option<Account>> {
        Ok(Some(self.account))
    }

    fn create_user(&self, _: &str, _: &Path) -> Result<Account> {
        Ok(self.account)
    }

    fn register_service(&self, _: &Path) -> Result<()> {
        self.calls.borrow_mut().push("register");
        self.registered.set(true);
        Ok(())
    }

    fn service_registered(&self) -> Result<bool> {
        Ok(self.registered.get())
    }

    fn restart_service(&self) -> Result<()> {
        self.calls.borrow_mut().push("restart");
        self.running.set(true);
        Ok(())
    }

    fn stop_service(&self) -> Result<()> {
        self.running.set(false);
        Ok(())
    }

    fn service_running(&self) -> Result<bool> {
        Ok(self.running.get())
    }

    fn service_instance(&self) -> Result<Option<(u32, u64)>> {
        Ok(self.running.get().then_some((42, 0)))
    }

    fn run_as(
        &self,
        account: &Account,
        env: &[(String, String)],
        program: &Path,
        args: &[String],
    ) -> Result<RunOutput> {
        self.probe.seen.borrow_mut().extend(args.iter().cloned());
        self.probe
            .seen
            .borrow_mut()
            .extend(env.iter().map(|(key, value)| format!("{key}={value}")));
        if args.first().map(String::as_str) == Some("preflight")
            && let Some((success, output)) = self.probe.preflight.borrow().clone()
        {
            return Ok(RunOutput { success, output });
        }
        system::run_as(account, env, program, args)
    }
}

/// A tiny executable that runs the real Bazel-built binary named by `variable`.
fn real(variable: &str) -> Vec<u8> {
    let binary = fs::canonicalize(std::env::var(variable).unwrap()).unwrap();
    format!("#!/bin/sh\nexec '{}' \"$@\"\n", binary.display()).into_bytes()
}

struct Installed {
    root: tempfile::TempDir,
    host: Fake,
}

impl Installed {
    /// A host with the signed package installed and nothing enrolled.
    fn new(hub: &Hub) -> Self {
        Self::with_url(hub, &hub.url)
    }

    fn with_url(hub: &Hub, url: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let host = Fake::new();
        let signer = SigningKey::from_text(SEED).unwrap();
        let platform = Platform::host().unwrap();
        let built = build(real("SPOKE_BIN"), real("INSTALLER_BIN"), "1.0.1", platform, URL_BASE).unwrap();
        let manifest = built.manifest.to_bytes();
        let signature = signer.sign(&manifest);
        let layout = Layout {
            platform,
            user: install::DEFAULT_USER.into(),
            root: Some(root.path().to_path_buf()),
        };
        let request = Request::builder()
            .manifest(&manifest)
            .signature(&signature)
            .package(&built.package)
            .trusted_key(signer.public())
            .host_id(HOST_ID)
            .hub_url(url)
            .hub_ca(hub.ca_pem.as_bytes())
            .platform(platform)
            .layout(layout)
            .health(INSTANT)
            .build();
        let report = install::install(&host, &request).unwrap();
        assert!(!report.enrolled, "installing enrolls nothing");
        assert_eq!(host.calls(), ["register"], "registered, never started");
        Self { root, host }
    }

    fn layout(&self) -> Layout {
        Layout {
            platform: Platform::host().unwrap(),
            user: install::DEFAULT_USER.into(),
            root: Some(self.root.path().to_path_buf()),
        }
    }

    fn config(&self) -> PathBuf {
        self.layout().on_disk(&self.layout().config_path())
    }

    fn credentials(&self) -> PathBuf {
        self.layout().on_disk(&self.layout().state_dir().join("credentials"))
    }

    fn run(&self, token: &str, replace: bool, repos: &[String]) -> Result<bootstrap::Report, Failure> {
        let installer = PathBuf::from("/nonexistent/boss-spoke-package");
        let config = self.config();
        let options = Options::builder()
            .config(&config)
            .root(self.root.path().to_path_buf())
            .token(token)
            .replace_identity(replace)
            .repos(repos)
            .health(INSTANT)
            .fallback_installer(&installer)
            .build();
        bootstrap::bootstrap(&self.host, &options)
    }

    fn leaf_der(&self) -> Vec<u8> {
        let pem = fs::read_to_string(self.credentials().join("cert.pem")).unwrap();
        let body: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();
        base64::engine::general_purpose::STANDARD.decode(body).unwrap()
    }

    fn mode(&self, name: &str) -> u32 {
        fs::metadata(self.credentials().join(name)).unwrap().mode() & 0o777
    }

    fn enrolled_files(&self) -> bool {
        ["cert.pem", "key.pem"]
            .iter()
            .all(|n| self.credentials().join(n).is_file())
    }

    fn no_token_left(&self) {
        assert!(
            !self.credentials().join(bootstrap::TOKEN_FILE).exists(),
            "the handed-over token must not outlive the attempt"
        );
    }
}

fn authorize(hub: &Hub, der: Vec<u8>) -> Result<boss_host_enrollment::AuthorizedHost, Denial> {
    hub.credentials
        .authorize(&[rustls::pki_types::CertificateDer::from(der)])
}

// ---------------------------------------------------------------------------

#[test]
fn the_bootstrap_and_the_hub_agree_on_the_token_format_and_job() {
    assert_eq!(bootstrap::MANUAL_JOB_ID, MANUAL_JOB_ID);
    let hub = Hub::start();
    let text = hub.mint("any-host");
    assert_eq!(bootstrap::decode_token(&format!("{text}\n")).unwrap().len(), 32);
}

#[test]
fn a_minted_token_enrolls_the_host_starts_its_service_and_is_checked_as_the_service_user() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    let token = hub.mint(HOST_ID);

    let report = host.run(&token, false, &[]).expect("enrolled, started and checked");

    assert_eq!(
        host.host.calls(),
        ["register", "restart"],
        "the service was started once"
    );
    assert!(host.host.running.get());
    assert_eq!(host.mode("key.pem"), 0o600);
    assert_eq!(host.mode("cert.pem"), 0o600);
    host.no_token_left();

    let authorized = authorize(&hub, host.leaf_der()).expect("the hub accepts the installed certificate");
    assert_eq!(authorized.host_id, HOST_ID);
    assert_eq!(report.host_id, HOST_ID);
    assert_eq!(report.serial, authorized.serial);
    assert_eq!(
        hub.credentials.status(HOST_ID).unwrap().map(|s| s.state),
        Some(HostState::Enrolled)
    );
}

#[test]
fn replaying_a_spent_token_is_refused_and_the_installed_identity_is_untouched() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    let token = hub.mint(HOST_ID);
    let _ = host.run(&token, false, &[]);
    let identity = host.leaf_der();
    let restarts = host.host.calls().len();

    // Without --replace-identity the second run does not even try.
    assert_eq!(host.run(&token, false, &[]).unwrap_err(), Failure::AlreadyEnrolled);

    // With it, the hub refuses the spent token and nothing changes.
    let refused = host.run(&token, true, &[]).unwrap_err();
    assert_eq!(refused.code(), "enrollment_refused", "{refused}");
    assert!(refused.token_spent());
    assert_eq!(host.leaf_der(), identity, "the old certificate is still installed");
    assert_eq!(host.host.calls().len(), restarts, "no restart for a refused enrollment");
    host.no_token_left();
    authorize(&hub, identity).expect("and still accepted by the hub");
}

#[test]
fn a_new_token_replaces_an_identity_and_the_old_certificate_stops_working() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    let _ = host.run(&hub.mint(HOST_ID), false, &[]);
    let old = host.leaf_der();

    let replacement_token = hub.mint(HOST_ID);
    let _ = host.run(&replacement_token, true, &[]);
    let new = host.leaf_der();
    // A replacement crashed after publishing its key but before its certificate.
    // The durable identity is already new; retry must repair without a new token.
    std::fs::write(host.credentials().join("cert.pem"), "old interrupted certificate").unwrap();
    host.run(&replacement_token, false, &[])
        .expect("repair succeeds with the already-spent token");
    assert_eq!(host.leaf_der(), new, "retry recovered the durable replacement identity");
    assert_ne!(old, new);
    authorize(&hub, new.clone()).expect("the new identity works");
    assert_eq!(authorize(&hub, old).unwrap_err(), Denial::Superseded);

    // Re-running the very same replacement command repairs it too, without a token.
    // The marker an interrupted replacement leaves names the identity replaced.
    std::fs::write(host.credentials().join("cert.pem"), "interrupted again").unwrap();
    std::fs::write(
        host.credentials().join(bootstrap::REPLACING_MARKER),
        "an-earlier-serial",
    )
    .unwrap();
    host.run(&replacement_token, true, &[])
        .expect("re-running with --replace-identity repairs locally");
    assert_eq!(host.leaf_der(), new);
    assert!(!host.credentials().join(bootstrap::REPLACING_MARKER).exists());
}

#[test]
fn replacing_after_the_daemon_files_were_lost_spends_the_fresh_token() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    host.run(&hub.mint(HOST_ID), false, &[]).unwrap();
    let old = host.leaf_der();
    fs::remove_file(host.credentials().join("cert.pem")).unwrap();
    fs::remove_file(host.credentials().join("key.pem")).unwrap();
    host.run(&hub.mint(HOST_ID), true, &[])
        .expect("a fresh token replaces the identity");
    assert_ne!(host.leaf_der(), old, "the old identity was not republished");
    assert_eq!(authorize(&hub, old).unwrap_err(), Denial::Superseded);
    host.no_token_left();
}

#[test]
fn replacing_the_identity_without_repos_keeps_the_persisted_repository_checks() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    *host.host.probe.preflight.borrow_mut() = None;
    let repos = [format!("file://{}/does-not-exist.git", host.root.path().display())];
    let first = host.run(&hub.mint(HOST_ID), false, &repos).unwrap_err();
    assert!(matches!(first, Failure::PreflightFailed { .. }), "{first}");
    let before = host.leaf_der();
    let Failure::PreflightFailed { failed, .. } = host.run(&hub.mint(HOST_ID), true, &[]).unwrap_err() else {
        panic!("the replacement must reach the preflight");
    };
    assert!(failed.iter().any(|name| name == "repo_access"), "{failed:?}");
    assert_ne!(host.leaf_der(), before, "the replacement enrolled a new identity");
    let kept = fs::read(host.credentials().join("preflight-repos")).unwrap();
    assert_eq!(
        kept,
        repos[0].as_bytes(),
        "repository checks survive a replacement without --repo"
    );
}

#[test]
fn a_token_for_another_host_is_refused() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    let refused = host.run(&hub.mint("someone-else"), false, &[]).unwrap_err();
    assert_eq!(refused.code(), "enrollment_refused", "{refused}");
    assert!(!host.enrolled_files());
    assert_eq!(host.host.calls(), ["register"], "the service was never started");
    host.no_token_left();
}

#[test]
fn a_revoked_host_cannot_reconnect_or_enroll_with_a_token_minted_before() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    let _ = host.run(&hub.mint(HOST_ID), false, &[]);
    let identity = host.leaf_der();
    let unused = hub.mint(HOST_ID);

    hub.credentials.revoke(HOST_ID, "test", "retired").unwrap();

    assert_eq!(
        authorize(&hub, identity.clone()).unwrap_err(),
        Denial::Revoked,
        "reconnect"
    );
    let refused = host.run(&unused, true, &[]).unwrap_err();
    assert_eq!(refused.code(), "enrollment_refused", "enroll again: {refused}");
    assert_eq!(
        host.leaf_der(),
        identity,
        "the dead identity is not replaced by a refusal"
    );
    assert!(
        hub.credentials.issue_manual_token(HOST_ID, "test", false).is_err(),
        "no new token either"
    );
}

#[test]
fn a_malformed_token_is_rejected_before_anything_is_started_or_sent() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    let error = host.run("this is not a token", false, &[]).unwrap_err();
    assert_eq!(error.code(), "token_invalid");
    assert!(
        !error.to_string().contains("this is not a token"),
        "the token is never echoed"
    );
    assert!(!host.enrolled_files());
    assert_eq!(host.host.calls(), ["register"]);
    host.no_token_left();
}

#[test]
fn an_unreachable_hub_leaves_the_token_unspent_and_the_request_to_repeat() {
    let hub = Hub::start();
    let closed = Hub::closed_url();
    let host = Installed::with_url(&hub, &closed);
    let token = hub.mint(HOST_ID);

    let error = host.run(&token, false, &[]).unwrap_err();
    assert_eq!(error.code(), "hub_unreachable", "{error}");
    assert!(
        !error.token_spent(),
        "the person may run enroll again with the same token"
    );
    assert!(!host.enrolled_files());
    assert_eq!(host.host.calls(), ["register"]);
    host.no_token_left();
    assert!(
        host.credentials().join("pending-enroll").exists(),
        "the request is kept so a retry repeats it"
    );
    // The hub never saw it, so the token still works for this host.
    assert!(hub.credentials.status(HOST_ID).unwrap().is_some());
}

#[test]
fn enrolling_before_installing_the_package_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let host = Fake::new();
    let missing = dir.path().join("install.json");
    let installer = PathBuf::from("/nonexistent");
    let options = Options::builder()
        .config(&missing)
        .token("irrelevant")
        .fallback_installer(&installer)
        .build();
    let error = bootstrap::bootstrap(&host, &options).unwrap_err();
    assert_eq!(error.code(), "not_installed");
    assert!(host.calls().is_empty());
}

// ------------------------------------------------------- preflight failure

#[test]
fn a_failed_preflight_is_a_typed_failure_and_the_host_is_never_reported_ready() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    *host.host.probe.preflight.borrow_mut() = Some((
        false,
        "PASS tmux: 3.4\nFAIL python_pty: python3 cannot import termios\nFAIL claude_credentials: not logged in\n\
         failed_preflights: python_pty, claude_credentials\n"
            .to_owned(),
    ));

    let failure = host.run(&hub.mint(HOST_ID), false, &[]).unwrap_err();

    let Failure::PreflightFailed { failed, output } = &failure else {
        panic!("expected a preflight failure, got {failure}");
    };
    assert_eq!(failed, &["python_pty", "claude_credentials"]);
    assert!(output.contains("python3 cannot import termios"));
    assert_eq!(failure.code(), "preflight_failed");
    assert_eq!(failure.exit_code(), 17);
    assert!(failure.token_spent(), "the host did enroll; only its checks failed");

    // The host holds an identity and its service runs, so the daemon can report
    // its own failing checks to the hub. Bootstrap records nothing that says
    // the host is ready.
    assert!(host.enrolled_files());
    assert!(host.host.running.get());
    let etc = host.layout().on_disk(&host.layout().etc());
    for entry in fs::read_dir(etc)
        .unwrap()
        .chain(fs::read_dir(host.credentials()).unwrap())
    {
        let name = entry.unwrap().file_name().to_string_lossy().to_lowercase();
        assert!(!name.contains("ready"), "unexpected readiness marker {name}");
    }
}

#[test]
fn the_real_preflight_runs_as_the_service_user_and_reports_an_unreadable_repository() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    *host.host.probe.preflight.borrow_mut() = None;
    let repos = [format!("file://{}/does-not-exist.git", host.root.path().display())];

    let failure = host.run(&hub.mint(HOST_ID), false, &repos).unwrap_err();

    let Failure::PreflightFailed { failed, output } = &failure else {
        panic!("expected a preflight failure, got {failure}");
    };
    assert!(failed.iter().any(|name| name == "repo_access"), "{failed:?}\n{output}");
    assert!(output.contains("FAIL repo_access"), "{output}");
    assert!(
        !output.contains("does-not-exist"),
        "a failing repository is reported as a count, never a URL: {output}"
    );
}

// ------------------------------------------------------------ the command

fn enroll_command(host: &Installed, token_file: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(std::env::var("INSTALLER_BIN").unwrap())
        .arg("enroll")
        .arg("--root")
        .arg(host.root.path())
        .arg("--config")
        .arg(host.config())
        .arg("--token-file")
        .arg(token_file)
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn the_command_exits_with_the_failure_code_and_deletes_a_spent_token_file() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    let token_file = host.root.path().join("token");
    fs::write(&token_file, format!("{}\n", hub.mint(HOST_ID))).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    let missing_repo = format!("file://{}/missing.git", host.root.path().display());

    let output = enroll_command(&host, &token_file, &["--repo", &missing_repo]);

    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(17), "{text}");
    assert!(text.contains("enrollment failed: preflight_failed"), "{text}");
    assert!(text.contains("failed_preflights:"), "{text}");
    assert!(text.contains("enrollment does not enable dispatch"), "{text}");
    assert!(!token_file.exists(), "the token was spent, so its file is gone");
}

#[test]
fn the_command_keeps_the_token_file_when_the_hub_was_unreachable() {
    let hub = Hub::start();
    let host = Installed::with_url(&hub, &Hub::closed_url());
    let token_file = host.root.path().join("token");
    fs::write(&token_file, hub.mint(HOST_ID)).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();

    let output = enroll_command(&host, &token_file, &[]);

    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(13), "{text}");
    assert!(text.contains("enrollment failed: hub_unreachable"), "{text}");
    assert!(token_file.exists(), "unspent, so the person can try again");
}

#[test]
fn the_command_refuses_a_token_file_other_users_can_read() {
    let hub = Hub::start();
    let host = Installed::new(&hub);
    let token_file = host.root.path().join("token");
    fs::write(&token_file, hub.mint(HOST_ID)).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o644)).unwrap();

    let output = enroll_command(&host, &token_file, &[]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("readable by its owner only"));
    assert!(token_file.exists());
    assert!(!host.enrolled_files());
}

#[test]
fn the_token_never_appears_in_any_argument_or_environment_given_to_the_service_user() {
    let hub = Hub::start();
    let token = hub.mint(HOST_ID);
    let host = Installed::new(&hub);

    host.run(&token, false, &[]).expect("enrolled");

    let seen = host.host.probe.seen.borrow();
    assert!(
        seen.iter().any(|arg| arg == "--inline"),
        "the as-user step ran: {seen:?}"
    );
    assert!(seen.iter().all(|arg| !arg.contains(&token)), "{seen:?}");
}
