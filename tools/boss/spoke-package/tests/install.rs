//! The installer against a staging root, with the real `boss-spoke` and
//! `boss-spoke-package` binaries as the package contents. The host operations
//! that would need root (creating a user, driving systemd or launchd) are
//! recorded rather than performed.

use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use boss_spoke_package::install::{self, Account, Health, Host, InstallRecord, Layout, Report, Request, RunOutput};
use boss_spoke_package::manifest::{Platform, SigningKey, TrustedKey};
use boss_spoke_package::service::{self, Spec};
use boss_spoke_package::{build, system};

const URL_BASE: &str = "https://github.com/spinyfin/mono/releases/download/boss-spoke-v1.0.1";
const SEED: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
const CA: &[u8] = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
const INSTANT: Health = Health {
    deadline: Duration::from_millis(50),
    poll: Duration::from_millis(1),
    stable: Duration::ZERO,
};

struct ServiceFlags {
    running: Cell<bool>,
    registered: Cell<bool>,
    register_fails: Cell<bool>,
    restart_stays_up: bool,
}

struct Recording {
    account: Account,
    user_exists: Cell<bool>,
    service: ServiceFlags,
    calls: RefCell<Vec<&'static str>>,
    on_run_as: RefCell<Option<Box<dyn Fn()>>>,
}

impl Recording {
    fn new() -> Self {
        // SAFETY: geteuid and getegid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self {
            account: Account { uid, gid },
            user_exists: Cell::new(true),
            service: ServiceFlags {
                running: Cell::new(false),
                registered: Cell::new(false),
                register_fails: Cell::new(false),
                restart_stays_up: true,
            },
            calls: RefCell::default(),
            on_run_as: RefCell::default(),
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }
}

impl Host for Recording {
    fn lookup_user(&self, _: &str) -> Result<Option<Account>> {
        Ok(self.user_exists.get().then_some(self.account))
    }

    fn create_user(&self, _: &str, _: &Path) -> Result<Account> {
        self.calls.borrow_mut().push("create_user");
        self.user_exists.set(true);
        Ok(self.account)
    }

    fn register_service(&self, _: &Path) -> Result<()> {
        self.calls.borrow_mut().push("register");
        if self.service.register_fails.get() {
            anyhow::bail!("registration failed");
        }
        self.service.registered.set(true);
        Ok(())
    }

    fn service_registered(&self) -> Result<bool> {
        Ok(self.service.registered.get())
    }

    fn restart_service(&self) -> Result<()> {
        self.calls.borrow_mut().push("restart");
        self.service.running.set(self.service.restart_stays_up);
        Ok(())
    }

    fn service_running(&self) -> Result<bool> {
        Ok(self.service.running.get())
    }

    fn run_as(
        &self,
        account: &Account,
        env: &[(String, String)],
        program: &Path,
        args: &[String],
    ) -> Result<RunOutput> {
        // The hook runs after the storage preflight so a swap of `credentials`
        // for a symlink lands between the check and the later root write.
        let output = system::run_as(account, env, program, args);
        if let Some(hook) = self.on_run_as.borrow().as_ref() {
            hook();
        }
        output
    }
}

struct Release {
    manifest: Vec<u8>,
    signature: String,
    package: Vec<u8>,
}

fn key() -> SigningKey {
    SigningKey::from_text(SEED).unwrap()
}

/// A tiny executable that runs the real Bazel-built binary named by `variable`.
/// Packing and unpacking the binaries themselves is slow in an unoptimised test
/// build and proves nothing the byte-oriented package tests do not.
fn real(variable: &str, release_id: &str) -> Vec<u8> {
    let binary = fs::canonicalize(std::env::var(variable).unwrap()).unwrap();
    format!(
        "#!/bin/sh\n# release {release_id}\nexec '{}' \"$@\"\n",
        binary.display()
    )
    .into_bytes()
}

fn release_with(signer: &SigningKey, id: &str, platform: Platform) -> Release {
    let built = build(real("SPOKE_BIN", id), real("INSTALLER_BIN", id), id, platform, URL_BASE).unwrap();
    let manifest = built.manifest.to_bytes();
    Release {
        signature: signer.sign(&manifest),
        manifest,
        package: built.package,
    }
}

fn release(id: &str) -> Release {
    release_with(&key(), id, Platform::host().unwrap())
}

struct Fixture {
    root: tempfile::TempDir,
    host: Recording,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            host: Recording::new(),
        }
    }

    fn layout(&self) -> Layout {
        Layout {
            platform: Platform::host().unwrap(),
            user: install::DEFAULT_USER.into(),
            root: Some(self.root.path().to_path_buf()),
        }
    }

    fn at(&self, logical: PathBuf) -> PathBuf {
        self.layout().on_disk(&logical)
    }

    fn install_with(&self, release: &Release, trusted: Option<TrustedKey>) -> Result<Report> {
        let request = Request::builder()
            .manifest(&release.manifest)
            .signature(&release.signature)
            .package(&release.package)
            .maybe_trusted_key(trusted)
            .host_id("host-one")
            .hub_url("https://hub.example:7443")
            .hub_ca(CA)
            .platform(Platform::host().unwrap())
            .layout(self.layout())
            .health(INSTANT)
            .build();
        install::install(&self.host, &request)
    }

    fn install(&self, release: &Release) -> Result<Report> {
        self.install_with(release, Some(key().public()))
    }

    /// What enrollment leaves behind.
    fn enroll(&self) {
        let credentials = self.at(self.layout().state_dir().join("credentials"));
        fs::write(credentials.join("cert.pem"), b"cert").unwrap();
        fs::write(credentials.join("key.pem"), b"key").unwrap();
    }

    fn mode(&self, logical: PathBuf) -> u32 {
        fs::metadata(self.at(logical)).unwrap().mode() & 0o7777
    }

    fn current_target(&self) -> PathBuf {
        fs::read_link(self.at(self.layout().current())).unwrap()
    }

    fn nothing_was_written(&self) {
        let entries: Vec<_> = fs::read_dir(self.root.path()).unwrap().collect();
        assert!(entries.is_empty(), "unexpected files: {entries:?}");
    }
}

#[test]
fn a_fresh_install_lays_out_owner_only_state_and_enrolls_nothing() {
    let fixture = Fixture::new();
    let layout = fixture.layout();
    let report = fixture.install(&release("1.0.1")).unwrap();

    assert_eq!(report.release_id, "1.0.1");
    assert!(!report.enrolled && !report.restarted);
    assert_eq!(fixture.host.calls(), ["register"], "registered, never started");

    // The service user's state is private; system files are not writable by it.
    for dir in [
        layout.state_dir(),
        layout.state_dir().join("credentials"),
        layout.state_dir().join("homes"),
    ] {
        assert_eq!(fixture.mode(dir.clone()), 0o700, "{}", dir.display());
    }
    assert_eq!(fixture.mode(layout.state_dir().join("credentials/ca.pem")), 0o600);
    for path in [
        layout.config_path(),
        layout.definition(),
        layout.etc().join("release-verify-key.pub"),
    ] {
        assert_eq!(fixture.mode(path.clone()), 0o644, "{}", path.display());
    }
    assert!(
        !fixture.at(layout.state_dir().join("credentials/cert.pem")).exists(),
        "no certificate: not enrolled"
    );

    assert_eq!(fixture.current_target(), Path::new("releases/1.0.1"));
    let daemon = fixture.at(layout.current().join("bin/boss-spoke"));
    assert!(fs::metadata(&daemon).unwrap().permissions().mode() & 0o111 != 0);
    assert!(Command::new(&daemon).arg("--help").output().unwrap().status.success());

    let record = InstallRecord::read(&fixture.at(layout.config_path())).unwrap();
    assert_eq!(
        (record.release_id.as_str(), record.host_id.as_str()),
        ("1.0.1", "host-one")
    );
}

#[test]
fn installing_again_changes_and_restarts_nothing() {
    let fixture = Fixture::new();
    let first = release("1.0.1");
    fixture.install(&first).unwrap();
    fixture.enroll();
    fixture.host.service.running.set(true);

    let report = fixture.install(&first).unwrap();
    assert!(report.changed.is_empty(), "{:?}", report.changed);
    assert!(!report.restarted);
    assert_eq!(fixture.host.calls(), ["register"]);
}

#[test]
fn a_missing_service_user_is_created_once() {
    let fixture = Fixture::new();
    fixture.host.user_exists.set(false);
    let report = fixture.install(&release("1.0.1")).unwrap();
    assert!(report.changed.contains(&"service user created"));
    fixture.install(&release("1.0.1")).unwrap();
    assert_eq!(
        fixture
            .host
            .calls()
            .iter()
            .filter(|call| **call == "create_user")
            .count(),
        1
    );
}

#[test]
fn an_upgrade_switches_current_and_restarts_an_enrolled_host_keeping_its_state() {
    let fixture = Fixture::new();
    let state = fixture.layout().state_dir();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    // Live state that must outlast a restart: the journal and the tmux socket.
    fs::write(fixture.at(state.join("journal.db")), b"journal").unwrap();
    fs::write(fixture.at(state.join("tmux.sock")), b"tmux").unwrap();

    let report = fixture.install(&release("1.0.2")).unwrap();

    assert!(report.changed.contains(&"current release switched"));
    assert!(report.enrolled && report.restarted);
    assert_eq!(fixture.host.calls(), ["register", "restart"]);
    assert_eq!(fixture.current_target(), Path::new("releases/1.0.2"));
    assert!(
        fixture.at(fixture.layout().release_dir("1.0.1")).is_dir(),
        "the previous release stays for a manual reinstall"
    );
    assert_eq!(fs::read(fixture.at(state.join("journal.db"))).unwrap(), b"journal");
    assert_eq!(fs::read(fixture.at(state.join("tmux.sock"))).unwrap(), b"tmux");
}

#[test]
fn an_enrolled_host_whose_service_is_down_is_started_without_other_changes() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    let report = fixture.install(&release("1.0.1")).unwrap();
    assert!(report.changed.is_empty() && report.restarted);
    assert_eq!(fixture.host.calls(), ["register", "restart"]);
}

#[test]
fn a_new_hub_ca_restarts_an_enrolled_host_but_a_new_host_id_is_refused() {
    let fixture = Fixture::new();
    let first = release("1.0.1");
    fixture.install(&first).unwrap();
    fixture.enroll();
    fixture.host.service.running.set(true);

    let rotated = b"-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----\n";
    let request = |host_id: &'static str, ca: &'static [u8]| {
        Request::builder()
            .manifest(&first.manifest)
            .signature(&first.signature)
            .package(&first.package)
            .host_id(host_id)
            .hub_url("https://hub.example:7443")
            .hub_ca(ca)
            .platform(Platform::host().unwrap())
            .layout(fixture.layout())
            .health(INSTANT)
            .build()
    };
    let report = install::install(&fixture.host, &request("host-one", rotated)).unwrap();
    assert_eq!(report.changed, ["hub trust installed"]);
    assert!(report.restarted, "the daemon reads its CA only at startup");

    let error = install::install(&fixture.host, &request("host-two", rotated)).unwrap_err();
    assert!(error.to_string().contains("installed as host-one"), "{error}");
}

#[test]
fn a_service_that_does_not_stay_up_is_reported_and_nothing_is_rolled_back() {
    let mut fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    fixture.host.service.restart_stays_up = false;

    let error = fixture.install(&release("1.0.2")).unwrap_err().to_string();
    assert!(error.contains("did not stay running") && error.contains("reinstall the previous signed package"));
    assert_eq!(fixture.current_target(), Path::new("releases/1.0.2"));
    assert!(fixture.at(fixture.layout().release_dir("1.0.1")).is_dir());
}

#[test]
fn the_first_install_pins_the_release_key_and_later_installs_keep_it() {
    let fixture = Fixture::new();
    let first = release("1.0.1");

    let error = fixture.install_with(&first, None).unwrap_err().to_string();
    assert!(error.contains("--trusted-key"), "{error}");
    fixture.nothing_was_written();

    let report = fixture.install_with(&first, Some(key().public())).unwrap();
    assert_eq!(report.key_fingerprint, key().public().fingerprint());
    // Without a key, the pinned one verifies the next release.
    fixture.install_with(&release("1.0.2"), None).unwrap();

    let (_, other) = SigningKey::generate().unwrap();
    let error = fixture
        .install_with(&release("1.0.2"), Some(other))
        .unwrap_err()
        .to_string();
    assert!(error.contains("trusted_key_changed"), "{error}");
}

#[test]
fn a_release_signed_by_another_key_installs_nothing() {
    let fixture = Fixture::new();
    let (seed, _) = SigningKey::generate().unwrap();
    let forged = release_with(
        &SigningKey::from_text(&seed).unwrap(),
        "1.0.1",
        Platform::host().unwrap(),
    );
    let error = fixture.install(&forged).unwrap_err().to_string();
    assert!(error.contains("manifest_sig"), "{error}");
    fixture.nothing_was_written();
    assert!(fixture.host.calls().is_empty());
}

#[test]
fn a_package_for_another_platform_installs_nothing() {
    let fixture = Fixture::new();
    let other = Platform::ALL
        .into_iter()
        .find(|platform| *platform != Platform::host().unwrap())
        .unwrap();
    let error = fixture
        .install(&release_with(&key(), "1.0.1", other))
        .unwrap_err()
        .to_string();
    assert!(error.contains("platform_mismatch"), "{error}");
    fixture.nothing_was_written();
}

#[test]
fn a_package_that_fails_its_digest_installs_nothing() {
    let fixture = Fixture::new();
    let mut tampered = release("1.0.1");
    let last = tampered.package.len() - 1;
    tampered.package[last] ^= 1;
    let error = fixture.install(&tampered).unwrap_err().to_string();
    assert!(error.contains("digest_mismatch"), "{error}");
    fixture.nothing_was_written();
}

#[test]
fn a_release_id_is_never_reused_for_different_contents() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    // Same id, different bytes: the wrapper's comment names another release.
    let built = build(
        real("SPOKE_BIN", "other"),
        real("INSTALLER_BIN", "1.0.1"),
        "1.0.1",
        Platform::host().unwrap(),
        URL_BASE,
    )
    .unwrap();
    let manifest = built.manifest.to_bytes();
    let again = Release {
        signature: key().sign(&manifest),
        manifest,
        package: built.package,
    };
    let error = fixture.install(&again).unwrap_err().to_string();
    assert!(error.contains("never reused"), "{error}");
    assert_eq!(fixture.current_target(), Path::new("releases/1.0.1"));
}

#[test]
fn a_state_directory_that_is_not_owner_only_is_refused_and_left_alone() {
    let fixture = Fixture::new();
    let state = fixture.at(fixture.layout().state_dir());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&state)
        .unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();

    let error = fixture.install(&release("1.0.1")).unwrap_err().to_string();
    assert!(error.contains("owner-only"), "{error}");
    assert_eq!(
        fs::metadata(&state).unwrap().mode() & 0o777,
        0o755,
        "the installer does not chmod user-controlled paths"
    );
}

#[test]
fn a_state_directory_that_is_a_symlink_is_refused() {
    let fixture = Fixture::new();
    let state = fixture.at(fixture.layout().state_dir());
    fs::DirBuilder::new()
        .recursive(true)
        .create(state.parent().unwrap())
        .unwrap();
    let elsewhere = fixture.root.path().join("elsewhere");
    fs::DirBuilder::new().mode(0o700).create(&elsewhere).unwrap();
    symlink(&elsewhere, &state).unwrap();

    let error = fixture.install(&release("1.0.1")).unwrap_err().to_string();
    assert!(error.contains("not a symlink"), "{error}");
}

#[test]
fn a_homes_root_the_service_user_cannot_write_stops_the_install_before_anything_points_at_it() {
    let fixture = Fixture::new();
    let homes = fixture.at(fixture.layout().state_dir().join("homes"));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&homes)
        .unwrap();
    fs::set_permissions(&homes, fs::Permissions::from_mode(0o500)).unwrap();
    // The state directory above it is created by the installer with the right mode.
    fs::set_permissions(homes.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();

    let error = fixture.install(&release("1.0.1")).unwrap_err().to_string();
    assert!(
        error.contains("cannot use its state directory") && error.contains("writable"),
        "{error}"
    );
    assert!(
        !fixture.at(fixture.layout().current()).exists(),
        "current is not switched"
    );
    assert!(
        !fixture.at(fixture.layout().definition()).exists(),
        "no service definition is written"
    );
    assert!(fixture.host.calls().is_empty());
}

#[test]
fn the_installed_preflight_runs_in_the_service_environment_and_refuses_a_missing_pin() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    let layout = fixture.layout();
    let spec = layout.spec("host-one", "https://hub.example:7443");
    let installed = fixture.at(layout.current().join("bin/boss-spoke-package"));
    let state = fixture.at(layout.state_dir());
    let preflight = |env: Vec<(String, String)>| {
        Command::new(&installed)
            .args(["preflight", "--inline", "--storage-only", "--state-dir"])
            .arg(&state)
            .env_clear()
            .envs(env)
            .output()
            .unwrap()
    };

    let pinned = preflight(layout.disk_environment(&spec));
    let output = String::from_utf8_lossy(&pinned.stdout);
    assert!(pinned.status.success(), "{output}");
    assert!(output.contains("PASS private_homes") && output.contains("PASS private_storage"));

    // The shared-temp default is never accepted: no pin, no pass.
    let unpinned = preflight(vec![("TMPDIR".into(), "/tmp".into())]);
    let output = String::from_utf8_lossy(&unpinned.stdout);
    assert!(!unpinned.status.success());
    assert!(
        output.contains("FAIL private_homes") && output.contains("shared-temp"),
        "{output}"
    );
    assert!(output.contains("failed_preflights: private_homes"));
}

fn spec(host_id: &str, hub_url: &str) -> Spec {
    Layout {
        platform: Platform::LinuxAmd64,
        user: "boss-worker".into(),
        root: None,
    }
    .spec(host_id, hub_url)
}

#[test]
fn both_service_definitions_pin_the_homes_root_and_keep_workers_across_a_restart() {
    let spec = spec("host-one", "https://hub.example:7443");
    let homes = spec.homes_dir().display().to_string();
    assert_eq!(homes, "/var/lib/boss-worker/spoke/homes");
    assert!(
        homes.starts_with(&spec.state_dir.display().to_string()),
        "inside owner-only service state"
    );

    let unit = spec.render_systemd();
    assert!(unit.contains(&format!("Environment={}={homes}\n", service::HOMES_ENV)));
    assert!(unit.contains("KillMode=process"), "workers must outlive a restart");
    assert!(unit.contains("User=boss-worker") && unit.contains("UMask=0077"));
    assert!(unit.contains("ConditionPathExists=/var/lib/boss-worker/spoke/credentials/cert.pem"));
    assert!(unit.contains(
        "ExecStart=/opt/boss-spoke/current/bin/boss-spoke --state-dir /var/lib/boss-worker/spoke \
         --host-id host-one --hub https://hub.example:7443 --tls-dir /var/lib/boss-worker/spoke/credentials"
    ));

    let plist = spec.render_launchd();
    assert!(plist.contains(&format!(
        "<key>{}</key>\n\t\t<string>{homes}</string>",
        service::HOMES_ENV
    )));
    assert!(
        plist.contains("<key>AbandonProcessGroup</key>\n\t<true/>"),
        "workers must outlive a restart"
    );
    assert!(plist.contains("<key>UserName</key>\n\t<string>boss-worker</string>"));
    assert!(
        !unit.contains("Group=") && !plist.contains("GroupName"),
        "both managers use the account's primary group"
    );
    assert!(plist.contains("<key>PathState</key>") && plist.contains("cert.pem"));

    // The unit is made only of sections, comments and Key=Value lines.
    for line in unit.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let section = line.starts_with('[') && line.ends_with(']');
        assert!(
            section
                || line
                    .split_once('=')
                    .is_some_and(|(key, _)| key.chars().all(|c| c.is_ascii_alphanumeric())),
            "{line}"
        );
    }

    // Neither definition can fall back to a shared temp directory.
    for definition in [&unit, &plist] {
        assert!(!definition.contains("TMPDIR") && !definition.contains("/tmp"));
        assert!(!definition.contains("{{"));
    }
}

#[test]
fn service_definition_inputs_are_plain_tokens() {
    assert!(spec("host-one", "https://hub.example:7443").validate().is_ok());
    for (host, hub) in [
        ("host one", "https://hub.example"),
        ("host\nid", "https://hub.example"),
        ("host-one", "http://hub.example"),
        ("host-one", "https://hub.example\" --evil"),
        ("host-one", "https://hub example"),
        ("host-one", "https://%n.example"),
        ("", "https://hub.example"),
    ] {
        assert!(spec(host, hub).validate().is_err(), "{host:?} {hub:?}");
    }
    let mut bad_user = spec("host-one", "https://hub.example");
    bad_user.user = "Root; rm".into();
    assert!(bad_user.validate().is_err());
}

#[test]
fn service_definitions_omit_a_group_named_after_the_user() {
    let spec = Layout {
        platform: Platform::MacosArm64,
        user: "boss-worker".into(),
        root: None,
    }
    .spec("host-one", "https://hub.example:7443");
    let unit = spec.render_systemd();
    let plist = spec.render_launchd();
    // macOS service accounts have primary group `staff`, not a group
    // named after the user. Linux accounts can also have a different primary
    // group. Leaving Group/GroupName unset uses that primary group.
    assert!(unit.contains("User=boss-worker") && !unit.contains("Group="));
    assert!(plist.contains("<key>UserName</key>") && !plist.contains("GroupName"));
}

#[test]
fn a_failed_registration_is_retried_when_the_definition_is_unchanged() {
    let fixture = Fixture::new();
    fixture.host.service.register_fails.set(true);
    let first = release("1.0.1");
    let error = fixture.install(&first).unwrap_err().to_string();
    assert!(error.contains("registration failed"), "{error}");
    assert_eq!(fixture.host.calls(), ["register"]);
    assert!(!fixture.host.service.registered.get());

    fixture.host.service.register_fails.set(false);
    let report = fixture.install(&first).unwrap();
    assert!(report.changed.contains(&"service registration"));
    assert_eq!(
        fixture.host.calls().iter().filter(|call| **call == "register").count(),
        2
    );
}

#[test]
fn swapping_credentials_for_a_symlink_during_preflight_stops_the_install() {
    let fixture = Fixture::new();
    let layout = fixture.layout();
    let credentials = fixture.at(layout.state_dir().join("credentials"));
    let elsewhere = fixture.root.path().join("root-controlled");
    fs::create_dir(&elsewhere).unwrap();
    let elsewhere_for_assert = elsewhere.clone();
    fixture.host.on_run_as.replace(Some(Box::new(move || {
        let _ = fs::remove_dir(&credentials);
        symlink(&elsewhere, &credentials).unwrap();
    })));

    let error = fixture.install(&release("1.0.1")).unwrap_err();
    let detail = format!("{error:#}");
    assert!(
        detail.contains("symlink") || detail.contains("replaced after it was verified"),
        "{detail}"
    );
    assert!(!elsewhere_for_assert.join("ca.pem").exists());
}

#[test]
fn a_writable_or_symlinked_pinned_key_is_refused() {
    let fixture = Fixture::new();
    let first = release("1.0.1");
    fixture.install(&first).unwrap();
    let key_path = fixture.at(fixture.layout().etc().join("release-verify-key.pub"));

    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o666)).unwrap();
    let error = format!("{:#}", fixture.install(&release("1.0.2")).unwrap_err());
    assert!(error.contains("writable by others"), "{error}");

    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
    let real = fixture.root.path().join("other.pub");
    fs::copy(&key_path, &real).unwrap();
    fs::remove_file(&key_path).unwrap();
    symlink(&real, &key_path).unwrap();
    let error = format!("{:#}", fixture.install(&release("1.0.2")).unwrap_err());
    assert!(error.contains("symlink"), "{error}");
}

#[test]
fn a_trust_directory_writable_by_others_is_refused() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    let etc = fixture.at(fixture.layout().etc());
    fs::set_permissions(&etc, fs::Permissions::from_mode(0o775)).unwrap();
    let error = fixture.install(&release("1.0.2")).unwrap_err().to_string();
    assert!(error.contains("writable by others"), "{error}");
}

#[test]
fn a_corrupt_install_record_does_not_bypass_the_host_id_guard() {
    let fixture = Fixture::new();
    let etc = fixture.at(fixture.layout().etc());
    fs::create_dir_all(&etc).unwrap();
    fs::set_permissions(&etc, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(etc.join("install.json"), b"{not json").unwrap();

    let error = fixture.install(&release("1.0.1")).unwrap_err().to_string();
    assert!(error.contains("inspect") && error.contains("install.json"), "{error}");
    assert!(
        !fixture.at(fixture.layout().current()).exists(),
        "a corrupt record must not be overwritten as a first install"
    );
}

#[test]
fn changed_definition_retries_registration_and_restart_for_an_enabled_service() {
    let fixture = Fixture::new();
    let first = release("1.0.1");
    fixture.install(&first).unwrap();
    fixture.enroll();
    fixture.host.service.running.set(true);
    fixture.host.service.register_fails.set(true);
    let request = Request::builder()
        .manifest(&first.manifest)
        .signature(&first.signature)
        .package(&first.package)
        .host_id("host-one")
        .hub_url("https://new-hub.example:7443")
        .hub_ca(CA)
        .platform(Platform::host().unwrap())
        .layout(fixture.layout())
        .health(INSTANT)
        .build();
    assert!(
        install::install(&fixture.host, &request)
            .unwrap_err()
            .to_string()
            .contains("registration failed")
    );
    assert!(fixture.host.service.registered.get());
    assert!(fixture.host.service.running.get());
    fixture.host.service.register_fails.set(false);
    let report = install::install(&fixture.host, &request).unwrap();
    assert!(report.restarted);
    assert_eq!(fixture.host.calls(), ["register", "register", "register", "restart"]);
    let report = install::install(&fixture.host, &request).unwrap();
    assert!(!report.restarted);
    assert!(report.changed.is_empty());
}
