//! Privileged update helper against a staging root. Host operations that would
//! need root are recorded rather than performed.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use boss_spoke_package::install::{self, Account, Health, Host, InstallRecord, Layout, Report, Request, RunOutput};
use boss_spoke_package::manifest::{self, Manifest, Platform, SigningKey};
use boss_spoke_package::update::{self, AlwaysReady, Apply, FileProbe, MapFetcher, NeverReady, Status};
use boss_spoke_package::{build, system};

const URL_BASE: &str = "https://github.com/spinyfin/mono/releases/download/boss-spoke-v1.0.1";
const SEED: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
const CA: &[u8] = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
const INSTANT: Health = Health {
    deadline: Duration::from_millis(50),
    poll: Duration::from_millis(1),
    stable: Duration::ZERO,
};

struct Recording {
    account: Account,
    user_exists: Cell<bool>,
    running: Cell<bool>,
    registered: Cell<bool>,
    restart_fails: Cell<bool>,
    calls: RefCell<Vec<&'static str>>,
}

impl Recording {
    fn new() -> Self {
        // SAFETY: geteuid and getegid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self {
            account: Account { uid, gid },
            user_exists: Cell::new(true),
            running: Cell::new(false),
            registered: Cell::new(false),
            restart_fails: Cell::new(false),
            calls: RefCell::default(),
        }
    }
}

impl Host for Recording {
    fn service_instance(&self) -> Result<Option<(u32, u64)>> {
        Ok(self.running.get().then_some((42, 0)))
    }
    fn lookup_user(&self, _: &str) -> Result<Option<Account>> {
        Ok(self.user_exists.get().then_some(self.account))
    }
    fn create_user(&self, _: &str, _: &Path) -> Result<Account> {
        self.user_exists.set(true);
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
        if self.restart_fails.get() {
            self.restart_fails.set(false);
            anyhow::bail!("restart failed");
        }
        self.running.set(true);
        Ok(())
    }
    fn stop_service(&self) -> Result<()> {
        self.calls.borrow_mut().push("stop");
        self.running.set(false);
        Ok(())
    }
    fn service_running(&self) -> Result<bool> {
        Ok(self.running.get())
    }
    fn run_as(
        &self,
        account: &Account,
        env: &[(String, String)],
        program: &Path,
        args: &[String],
    ) -> Result<RunOutput> {
        system::run_as(account, env, program, args)
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

fn real(variable: &str, release_id: &str) -> Vec<u8> {
    let binary = fs::canonicalize(std::env::var(variable).unwrap()).unwrap();
    format!(
        "#!/bin/sh\n# release {release_id}\nexec '{}' \"$@\"\n",
        binary.display()
    )
    .into_bytes()
}

fn release_with(id: &str, platform: Platform) -> Release {
    let built = build(real("SPOKE_BIN", id), real("INSTALLER_BIN", id), id, platform, URL_BASE).unwrap();
    let manifest = built.manifest.to_bytes();
    Release {
        signature: key().sign(&manifest),
        manifest,
        package: built.package,
    }
}

fn release(id: &str) -> Release {
    release_with(id, Platform::host().unwrap())
}

fn write_journal(path: &Path, version: u32) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "user_version", version).unwrap();
}

fn write_journal_wal(path: &Path, version: u32) {
    write_journal(path, version);
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "user_version", version).unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS keep (n INTEGER); INSERT INTO keep(n) VALUES (1);")
        .unwrap();
    drop(conn);
    let mut header = fs::read(path).unwrap();
    if header.len() >= 64 {
        header[60..64].copy_from_slice(&0u32.to_be_bytes());
        fs::write(path, header).unwrap();
    }
}

fn tmux_bin() -> PathBuf {
    fs::canonicalize(std::env::var("TMUX_BIN").unwrap_or_else(|_| "tmux".into())).unwrap_or_else(|_| PathBuf::from("tmux"))
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

    fn install(&self, release: &Release) -> Result<Report> {
        let request = Request::builder()
            .manifest(&release.manifest)
            .signature(&release.signature)
            .package(&release.package)
            .trusted_key(key().public())
            .host_id("host-one")
            .hub_url("https://hub.example:7443")
            .hub_ca(CA)
            .platform(Platform::host().unwrap())
            .layout(self.layout())
            .health(INSTANT)
            .build();
        install::install(&self.host, &request)
    }

    fn enroll(&self) {
        let credentials = self.at(self.layout().state_dir().join("credentials"));
        fs::write(credentials.join("cert.pem"), b"cert").unwrap();
        fs::write(credentials.join("key.pem"), b"key").unwrap();
    }

    fn current(&self) -> PathBuf {
        fs::read_link(self.at(self.layout().current())).unwrap()
    }

    fn seed_workers(&self) {
        let state = self.layout().state_dir();
        write_journal(&self.at(state.join("journal.db")), 6);
        let socket = self.at(state.join("tmux.sock"));
        let tmux = tmux_bin();
        let started = std::process::Command::new(&tmux)
            .args([
                "-S",
                socket.to_str().unwrap(),
                "new-session",
                "-d",
                "-s",
                "worker",
                "sleep",
                "3600",
            ])
            .status();
        if started.map(|status| status.success()).unwrap_or(false) {
            return;
        }
        fs::write(&socket, b"tmux").unwrap();
    }

    fn tmux_alive(&self) -> bool {
        let socket = self.at(self.layout().state_dir().join("tmux.sock"));
        std::process::Command::new(tmux_bin())
            .args(["-S", socket.to_str().unwrap(), "has-session", "-t", "worker"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

fn offer(release: &Release) -> (update::Request, MapFetcher) {
    let parsed: Manifest = serde_json::from_slice(&release.manifest).unwrap();
    let platform = Platform::parse(&parsed.platform).unwrap();
    let manifest_url = format!("{URL_BASE}/boss-spoke-{}.manifest.json", platform.as_str());
    let mut files = HashMap::new();
    files.insert(manifest_url.clone(), release.manifest.clone());
    files.insert(
        format!("{manifest_url}.sig"),
        release.signature.trim().as_bytes().to_vec(),
    );
    files.insert(parsed.package.url, release.package.clone());
    (
        update::Request {
            schema: 1,
            command_id: "cmd-1".into(),
            release_id: parsed.release_id,
            manifest_url,
            manifest_sha256: manifest::sha256_hex(&release.manifest),
        },
        MapFetcher { files },
    )
}

fn apply(
    fixture: &Fixture,
    request: &update::Request,
    fetcher: &MapFetcher,
    probe: &dyn update::HealthProbe,
) -> Result<Status> {
    update::write_request(&fixture.at(fixture.layout().request_path()), request).unwrap();
    update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher,
        probe,
        health: INSTANT,
        crash: None,
    })
}

fn write_request_and_map(fixture: &Fixture, request: &update::Request, fetcher: &MapFetcher) {
    update::write_request(&fixture.at(fixture.layout().request_path()), request).unwrap();
    let mut map = HashMap::new();
    for (url, bytes) in &fetcher.files {
        let file = fixture.root.path().join(format!("fetch-{}", map.len()));
        fs::write(&file, bytes).unwrap();
        map.insert(url.clone(), file.display().to_string());
    }
    fs::write(
        fixture.root.path().join("fixture-files.json"),
        serde_json::to_vec(&map).unwrap(),
    )
    .unwrap();
}

fn helper_crashed(fixture: &Fixture, _fetcher: &MapFetcher, point: &str) -> std::process::ExitStatus {
    let installer = fs::canonicalize(std::env::var("INSTALLER_BIN").unwrap()).unwrap();
    let config = fixture.at(fixture.layout().config_path());
    std::process::Command::new(installer)
        .args([
            "apply-update",
            "--config",
            config.to_str().unwrap(),
            "--root",
            fixture.root.path().to_str().unwrap(),
            "--fixture-files",
            fixture.root.path().join("fixture-files.json").to_str().unwrap(),
        ])
        .env("BOSS_SPOKE_UPDATE_CRASH", point)
        .status()
        .unwrap()
}

fn in_progress_phase(fixture: &Fixture) -> Option<String> {
    let path = fixture.at(fixture.layout().in_progress_path());
    let bytes = fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("phase")?.as_str().map(str::to_owned)
}

fn status_file(fixture: &Fixture) -> Status {
    serde_json::from_slice(&fs::read(fixture.at(fixture.layout().status_path())).unwrap()).unwrap()
}

#[test]
fn a_verified_upgrade_switches_current_and_keeps_workers() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    fixture.seed_workers();
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "ok");
    assert_eq!(status.running_release_id, "1.0.2");
    assert_eq!(fixture.current(), Path::new("releases/1.0.2"));
    assert_eq!(
        fs::read_link(fixture.at(fixture.layout().previous())).unwrap(),
        Path::new("releases/1.0.1")
    );
    let account = fixture.host.account;
    let version = update::journal_user_version(&fixture.layout(), &account)
        .unwrap()
        .unwrap();
    assert_eq!(version, 6);
    if fixture.tmux_alive() {
        assert!(fixture.at(fixture.layout().state_dir().join("tmux.sock")).exists());
    } else {
        assert_eq!(
            fs::read(fixture.at(fixture.layout().state_dir().join("tmux.sock"))).unwrap(),
            b"tmux"
        );
    }
    assert!(fixture.host.calls.borrow().contains(&"restart"));
    let record = InstallRecord::read(&fixture.at(fixture.layout().config_path())).unwrap();
    assert_eq!(record.release_id, "1.0.2");
    assert!(!fixture.at(fixture.layout().request_path()).exists());
    assert!(!fixture.at(fixture.layout().in_progress_path()).exists());
}

#[test]
fn a_bad_signature_or_digest_is_refused_before_switching() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    let next = release("1.0.2");
    let (request, mut fetcher) = offer(&next);
    fetcher
        .files
        .insert(format!("{}.sig", request.manifest_url), b"not-a-signature".to_vec());
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "manifest_sig");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));

    let (mut request, fetcher) = offer(&next);
    request.manifest_sha256 = "ab".repeat(32);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "digest_mismatch");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
}

#[test]
fn the_wrong_platform_is_refused() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    let other = Platform::ALL
        .into_iter()
        .find(|platform| *platform != Platform::host().unwrap())
        .unwrap();
    let next = release_with("1.0.2", other);
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "platform_mismatch");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
}

#[test]
fn an_unapproved_origin_is_refused() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    let next = release("1.0.2");
    let (mut request, fetcher) = offer(&next);
    request.manifest_url = "https://evil.example/manifest.json".into();
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "unapproved_origin");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));

    let (mut request, fetcher) = offer(&next);
    request.manifest_url = "https://github.com/spinyfin/mono/releases/../other/manifest.json".into();
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "unapproved_origin");
}

#[test]
fn an_incompatible_journal_is_refused_before_switching() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    write_journal(&fixture.at(fixture.layout().state_dir().join("journal.db")), 6);
    let mut next = release("1.0.2");
    let mut parsed: Manifest = serde_json::from_slice(&next.manifest).unwrap();
    parsed.oldest_journal_version = 7;
    next.manifest = parsed.to_bytes();
    next.signature = key().sign(&next.manifest);
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "journal_incompatible");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    assert!(!fixture.at(fixture.layout().release_dir("1.0.2")).exists());
}

#[test]
fn a_binary_that_fails_health_rolls_back_automatically() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    fixture.seed_workers();
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &NeverReady).unwrap();
    assert_eq!(status.result, "rolled_back");
    assert_eq!(status.code(), "health_failed");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    if !fixture.tmux_alive() {
        assert_eq!(
            fs::read(fixture.at(fixture.layout().state_dir().join("tmux.sock"))).unwrap(),
            b"tmux"
        );
    }
    let record = InstallRecord::read(&fixture.at(fixture.layout().config_path())).unwrap();
    assert_eq!(record.release_id, "1.0.1");
    assert!(!fixture.at(fixture.layout().in_progress_path()).exists());
}

fn recover(fixture: &Fixture, fetcher: &MapFetcher) -> Status {
    update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher,
        probe: &AlwaysReady,
        health: INSTANT,
        crash: None,
    })
    .unwrap()
}

#[test]
fn an_interrupted_switch_recovers_to_a_consistent_layout() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    fixture.seed_workers();
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    for point in ["after_stage", "after_switch", "after_restart", "after_rollback_marker", "after_rollback_switch"]
    {
        write_request_and_map(&fixture, &request, &fetcher);
        let status = helper_crashed(&fixture, &fetcher, point);
        assert!(!status.success(), "{point}: {status:?}");
        recover(&fixture, &fetcher);
        assert_eq!(fixture.current(), Path::new("releases/1.0.1"), "{point}");
        assert!(!fixture.at(fixture.layout().in_progress_path()).exists(), "{point}");
    }
    for point in ["after_persist_record", "after_status"] {
        write_request_and_map(&fixture, &request, &fetcher);
        let status = helper_crashed(&fixture, &fetcher, point);
        assert!(!status.success(), "{point}: {status:?}");
        recover(&fixture, &fetcher);
        assert_eq!(fixture.current(), Path::new("releases/1.0.2"), "{point}");
        assert!(!fixture.at(fixture.layout().in_progress_path()).exists(), "{point}");
        let record = InstallRecord::read(&fixture.at(fixture.layout().config_path())).unwrap();
        assert_eq!(record.release_id, "1.0.2", "{point}");
        fixture.install(&release("1.0.1")).unwrap();
        fixture.enroll();
    }
}

#[test]
fn the_helper_refuses_a_release_that_fails_the_pinned_key_and_root_paths_stay_protected() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    let mut next = release("1.0.2");
    let (seed, _) = SigningKey::generate().unwrap();
    let stranger = SigningKey::from_text(&seed).unwrap();
    next.signature = stranger.sign(&next.manifest);
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "manifest_sig");
    assert_eq!(status_file(&fixture).code(), "manifest_sig");

    let layout = fixture.layout();
    let current = fixture.at(layout.current());
    assert_eq!(fs::read_link(&current).unwrap(), Path::new("releases/1.0.1"));
    let parent = current.parent().unwrap();
    let parent_meta = fs::metadata(parent).unwrap();
    assert_eq!(parent_meta.uid(), fixture.host.account.uid);
    assert_eq!(parent_meta.mode() & 0o022, 0, "current's parent is writable by others");
    let release_dir = fixture.at(layout.release_dir("1.0.1"));
    let release_meta = fs::metadata(&release_dir).unwrap();
    assert_eq!(release_meta.uid(), fixture.host.account.uid);
    assert_eq!(release_meta.mode() & 0o022, 0, "release directory is writable by others");
    for path in [layout.pinned_key_path(), layout.helper_definition(), layout.config_path()] {
        let meta = fs::symlink_metadata(fixture.at(path.clone())).unwrap();
        assert_eq!(meta.uid(), fixture.host.account.uid, "{}", path.display());
        assert_eq!(meta.mode() & 0o022, 0, "{} is writable by others", path.display());
    }
}

#[test]
fn the_first_install_pins_the_package_origin_and_writes_the_helper() {
    let fixture = Fixture::new();
    let report = fixture.install(&release("1.0.1")).unwrap();
    assert!(report.changed.contains(&"update helper"));
    let record = InstallRecord::read(&fixture.at(fixture.layout().config_path())).unwrap();
    assert_eq!(record.package_origin, "https://github.com/spinyfin/mono/releases");
    assert!(fixture.at(fixture.layout().helper_definition()).is_file());
    if let Some(path) = fixture.layout().helper_path_definition() {
        assert!(fixture.at(path).is_file());
        let body = fs::read_to_string(fixture.at(path)).unwrap_or_default();
        let _ = body;
    }
    if !Platform::host().unwrap().is_macos() {
        let unit = fs::read_to_string(fixture.at(fixture.layout().helper_definition())).unwrap();
        assert!(unit.contains("TimeoutStartSec=600"), "{unit}");
    }
}

#[test]
fn a_malformed_request_is_refused_and_removed() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    let path = fixture.at(fixture.layout().request_path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{not-json").unwrap();
    let status = update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher: &MapFetcher {
            files: HashMap::new(),
        },
        probe: &AlwaysReady,
        health: INSTANT,
        crash: None,
    })
    .unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "request_malformed");
    assert!(!path.exists());
    assert_eq!(status_file(&fixture).code(), "request_malformed");
}

#[test]
fn a_restart_failure_after_switch_rolls_back() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    fixture.seed_workers();
    fixture.host.restart_fails.set(true);
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "rolled_back");
    assert!(status.error.contains("restart failed"), "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    assert!(!fixture.at(fixture.layout().in_progress_path()).exists());
}

#[test]
fn a_stale_health_file_from_the_previous_release_does_not_satisfy_the_probe() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    let layout = fixture.layout();
    let health = fixture.at(layout.health_path());
    fs::write(
        &health,
        serde_json::json!({
            "lock_ok": true,
            "journal_ok": true,
            "inventory_ok": true,
            "handshake_ok": true,
            "release_id": "1.0.1",
            "pid": 42,
            "nonce": "old"
        })
        .to_string(),
    )
    .unwrap();
    let probe = FileProbe {
        layout: &layout,
        account: fixture.host.account,
    };
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &probe).unwrap();
    assert_eq!(status.result, "rolled_back");
    assert_eq!(status.code(), "health_failed");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
}

#[test]
fn a_journal_version_committed_only_in_wal_is_visible_to_the_gate() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    write_journal_wal(&fixture.at(fixture.layout().state_dir().join("journal.db")), 6);
    let mut next = release("1.0.2");
    let mut parsed: Manifest = serde_json::from_slice(&next.manifest).unwrap();
    parsed.oldest_journal_version = 7;
    next.manifest = parsed.to_bytes();
    next.signature = key().sign(&next.manifest);
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "journal_incompatible");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
}

struct BumpJournal {
    path: PathBuf,
}

impl update::HealthProbe for BumpJournal {
    fn ready(&self, _: &str, _: u32, _: &str) -> Result<bool> {
        write_journal(&self.path, 99);
        Ok(false)
    }
}

#[test]
fn rollback_refuses_when_the_journal_migrated_past_previous() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    write_journal(&fixture.at(fixture.layout().state_dir().join("journal.db")), 6);
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let probe = BumpJournal {
        path: fixture.at(fixture.layout().state_dir().join("journal.db")),
    };
    let status = apply(&fixture, &request, &fetcher, &probe).unwrap();
    assert_eq!(status.result, "rollback_refused_journal_migrated");
    assert_eq!(fixture.current(), Path::new("releases/1.0.2"));
    assert_eq!(
        in_progress_phase(&fixture).as_deref(),
        Some("rollback_refused")
    );
}
