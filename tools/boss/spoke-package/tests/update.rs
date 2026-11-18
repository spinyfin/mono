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

struct RecordingFlags {
    user_exists: Cell<bool>,
    running: Cell<bool>,
    registered: Cell<bool>,
    restart_fails: Cell<bool>,
    extra: RecordingExtra,
}

struct RecordingExtra {
    stop: StopProgress,
    inspector_calls: Cell<u32>,
    inspector_fail_on: Cell<u32>,
}

/// What the service manager reports between `stop_service` and the next start.
struct StopProgress {
    /// Instance polls that still report a running daemon after `stop_service`.
    linger_polls: Cell<u32>,
    /// Polls of `service_instance` made while the daemon was being stopped.
    polls: Cell<u32>,
    active: Cell<bool>,
    /// When set, `stop_service` writes this journal version, simulating a
    /// candidate that commits a migration only as it is being stopped.
    journal: RefCell<Option<(PathBuf, u32)>>,
    /// `(stops so far, write journal on this 1-based stop; 0 = every stop)`.
    schedule: Cell<(u32, u32)>,
}

struct Recording {
    account: Account,
    flags: RecordingFlags,
    calls: RefCell<Vec<&'static str>>,
    handoffs: RefCell<Vec<String>>,
}

impl Recording {
    fn new() -> Self {
        // SAFETY: geteuid and getegid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Self {
            account: Account { uid, gid },
            flags: RecordingFlags {
                user_exists: Cell::new(true),
                running: Cell::new(false),
                registered: Cell::new(false),
                restart_fails: Cell::new(false),
                extra: RecordingExtra {
                    inspector_calls: Cell::new(0),
                    inspector_fail_on: Cell::new(0),
                    stop: StopProgress {
                        linger_polls: Cell::new(0),
                        polls: Cell::new(0),
                        active: Cell::new(false),
                        journal: RefCell::new(None),
                        schedule: Cell::new((0, 0)),
                    },
                },
            },
            calls: RefCell::default(),
            handoffs: RefCell::default(),
        }
    }
}

impl Host for Recording {
    fn service_instance(&self) -> Result<Option<(u32, u64)>> {
        let stop = &self.flags.extra.stop;
        if stop.active.get() {
            stop.polls.set(stop.polls.get() + 1);
            let left = stop.linger_polls.get();
            if left > 0 {
                stop.linger_polls.set(left - 1);
                return Ok(Some((42, 0)));
            }
            return Ok(None);
        }
        Ok(self.flags.running.get().then_some((42, 0)))
    }
    fn lookup_user(&self, _: &str) -> Result<Option<Account>> {
        Ok(self.flags.user_exists.get().then_some(self.account))
    }
    fn create_user(&self, _: &str, _: &Path) -> Result<Account> {
        self.flags.user_exists.set(true);
        Ok(self.account)
    }
    fn register_service(&self, _: &Path) -> Result<()> {
        self.calls.borrow_mut().push("register");
        self.flags.registered.set(true);
        Ok(())
    }
    fn service_registered(&self) -> Result<bool> {
        Ok(self.flags.registered.get())
    }
    fn restart_service(&self) -> Result<()> {
        self.calls.borrow_mut().push("restart");
        if self.flags.restart_fails.get() {
            self.flags.restart_fails.set(false);
            anyhow::bail!("restart failed");
        }
        self.flags.running.set(true);
        self.flags.extra.stop.active.set(false);
        Ok(())
    }
    fn stop_service(&self) -> Result<()> {
        self.calls.borrow_mut().push("stop");
        let (stops, nth) = self.flags.extra.stop.schedule.get();
        let n = stops + 1;
        self.flags.extra.stop.schedule.set((n, nth));
        if let Some((path, version)) = self.flags.extra.stop.journal.borrow().clone()
            && (nth == 0 || n == nth)
        {
            write_journal(&path, version);
        }
        self.flags.running.set(false);
        self.flags.extra.stop.active.set(true);
        Ok(())
    }
    fn service_running(&self) -> Result<bool> {
        Ok(self.flags.running.get())
    }
    fn run_as(
        &self,
        account: &Account,
        env: &[(String, String)],
        program: &Path,
        args: &[String],
    ) -> Result<RunOutput> {
        if args.iter().any(|arg| arg == "journal-version") {
            let n = self.flags.extra.inspector_calls.get() + 1;
            self.flags.extra.inspector_calls.set(n);
            let fail_on = self.flags.extra.inspector_fail_on.get();
            if fail_on != 0 && n == fail_on {
                return Ok(RunOutput {
                    success: false,
                    output: "injected inspector failure".into(),
                });
            }
        }
        system::run_as(account, env, program, args)
    }
    fn note_handoff(&self, logical: &Path) {
        self.handoffs.borrow_mut().push(logical.display().to_string());
    }
}

/// Fails the health contract for one release and passes for every other, so a
/// bad candidate is rolled back onto a release that is genuinely healthy.
struct FailsFor(&'static str);

impl update::HealthProbe for FailsFor {
    fn ready(&self, release_id: &str, _: u32, _: &str) -> Result<bool> {
        Ok(release_id != self.0)
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

/// A journal whose newest `user_version` exists only in the write-ahead log:
/// the main file keeps `base`, the committed WAL frame says `wal`, and the
/// returned connection stays open with checkpointing off so it stays that way.
fn write_journal_wal(path: &Path, base: u32, wal: u32) -> rusqlite::Connection {
    write_journal(path, base);
    let conn = rusqlite::Connection::open(path).unwrap();
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS keep (n INTEGER); INSERT INTO keep(n) VALUES (1);")
        .unwrap();
    conn.pragma_update(None, "user_version", wal).unwrap();
    conn
}

fn header_user_version(path: &Path) -> u32 {
    let header = fs::read(path).unwrap();
    u32::from_be_bytes(header[60..64].try_into().unwrap())
}

fn inspector() -> PathBuf {
    fs::canonicalize(std::env::var("INSTALLER_BIN").unwrap()).unwrap()
}

fn tmux_bin() -> PathBuf {
    fs::canonicalize(std::env::var("TMUX_BIN").unwrap_or_else(|_| "tmux".into()))
        .unwrap_or_else(|_| PathBuf::from("tmux"))
}

/// `host_tmux` is a real binary on macOS and an exit-127 stub on Linux
/// `bazel-any` agents that have no tmux package. Probe `-V` so upgrade
/// cases still run in CI; live-session survival is asserted only when
/// the host can actually start a server.
fn tmux_is_usable() -> bool {
    std::process::Command::new(tmux_bin())
        .arg("-V")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

struct Fixture {
    root: tempfile::TempDir,
    host: Recording,
    workers_seeded: Cell<bool>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            host: Recording::new(),
            workers_seeded: Cell::new(false),
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
        let report = install::install(&self.host, &request);
        // Provisioning calls are not part of what an upgrade asks of the host.
        self.host.calls.borrow_mut().clear();
        report
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
        if !tmux_is_usable() {
            return;
        }
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
            .output();
        match started {
            Ok(out) if out.status.success() => self.workers_seeded.set(true),
            Ok(out) => panic!(
                "tmux at {} failed to start a worker session: {}\n{}",
                tmux.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(err) => panic!("tmux at {} failed to spawn: {err}", tmux.display()),
        }
    }

    fn tmux_alive(&self) -> bool {
        let socket = self.at(self.layout().state_dir().join("tmux.sock"));
        std::process::Command::new(tmux_bin())
            .args(["-S", socket.to_str().unwrap(), "has-session", "-t", "worker"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn assert_workers_survived(&self, context: &str) {
        if !self.workers_seeded.get() {
            return;
        }
        assert!(self.tmux_alive(), "{context}");
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
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
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

fn helper_command(fixture: &Fixture) -> std::process::Command {
    let installer = fs::canonicalize(std::env::var("INSTALLER_BIN").unwrap()).unwrap();
    let config = fixture.at(fixture.layout().config_path());
    let mut command = std::process::Command::new(installer);
    command.args([
        "apply-update",
        "--config",
        config.to_str().unwrap(),
        "--root",
        fixture.root.path().to_str().unwrap(),
        "--fixture-files",
        fixture.root.path().join("fixture-files.json").to_str().unwrap(),
    ]);
    command
}

fn layout_is_consistent(fixture: &Fixture) {
    let record = InstallRecord::read(&fixture.at(fixture.layout().config_path())).unwrap();
    assert_eq!(
        fixture.current(),
        Path::new("releases").join(&record.release_id),
        "current must match install.json"
    );
    let phase = in_progress_phase(fixture);
    assert!(
        phase.is_none()
            || matches!(
                phase.as_deref(),
                Some("rollback_refused" | "rollback_failed" | "restore_failed")
            ),
        "in-progress marker left: {phase:?}"
    );
    let status = status_file(fixture);
    assert!(
        matches!(
            status.result.as_str(),
            "ok" | "rolled_back"
                | "rollback_failed"
                | "restore_failed"
                | "rollback_refused_journal_migrated"
                | "refused"
                | "failed"
                | "idle"
        ),
        "non-terminal status {}",
        status.result
    );
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
    let version = update::journal_user_version(&fixture.host, &fixture.layout(), &account, &inspector())
        .unwrap()
        .unwrap();
    assert_eq!(version, 6);
    fixture.assert_workers_survived("tmux worker session must survive the switch");
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
    // The daemon was stopped to read the journal; a refusal must not leave it
    // stopped, and the restart is the last thing the host was asked to do.
    assert_eq!(*fixture.host.calls.borrow(), ["stop", "restart"]);
    assert!(fixture.host.flags.running.get(), "the spoke must be running again");
    assert!(status.error.contains("service restarted on 1.0.1"), "{}", status.error);
    assert!(!fixture.at(fixture.layout().request_path()).exists());
    assert!(!fixture.at(fixture.layout().in_progress_path()).exists());
    assert_eq!(status_file(&fixture).code(), "journal_incompatible");
}

#[test]
fn a_binary_that_fails_health_rolls_back_automatically() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    fixture.seed_workers();
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &FailsFor("1.0.2")).unwrap();
    assert_eq!(status.result, "rolled_back");
    assert_eq!(status.code(), "health_failed");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    fixture.assert_workers_survived("tmux worker session must survive rollback");
    let record = InstallRecord::read(&fixture.at(fixture.layout().config_path())).unwrap();
    assert_eq!(record.release_id, "1.0.1");
    assert!(!fixture.at(fixture.layout().in_progress_path()).exists());
}

#[test]
fn an_interrupted_switch_recovers_to_a_consistent_layout() {
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    for point in [
        "after_prepare_marker",
        "after_stop",
        "after_stage",
        "after_switch",
        "after_restart",
        "after_persist_record",
        "after_status",
        "after_remove_request",
        "after_clear_in_progress",
    ] {
        crash_and_recover(&request, &fetcher, point, None);
    }
    for point in [
        "after_rollback_marker",
        "after_rollback_switch",
        "after_rollback_health",
    ] {
        crash_and_recover(&request, &fetcher, point, Some("only_installed"));
    }
    // A rollback that cannot prove the restored release healthy stays unresolved
    // instead of reporting success.
    for point in ["after_rollback_marker", "after_rollback_switch"] {
        crash_and_recover(&request, &fetcher, point, Some("never"));
    }
}

fn crash_and_recover(request: &update::Request, fetcher: &MapFetcher, point: &str, probe: Option<&str>) {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    fixture.seed_workers();
    write_request_and_map(&fixture, request, fetcher);
    let mut crashed = helper_command(&fixture);
    crashed.env("BOSS_SPOKE_UPDATE_CRASH", point);
    if let Some(probe) = probe {
        crashed.env("BOSS_SPOKE_UPDATE_PROBE", probe);
    }
    let crashed = crashed.status().unwrap();
    assert!(!crashed.success(), "{point} should abort: {crashed:?}");
    let mut restarted = helper_command(&fixture);
    if let Some(probe) = probe {
        restarted.env("BOSS_SPOKE_UPDATE_PROBE", probe);
    }
    let restarted = restarted.status().unwrap();
    assert!(restarted.success(), "{point} restart: {restarted:?}");
    layout_is_consistent(&fixture);
    fixture.assert_workers_survived(&format!("{point}: tmux worker must survive helper crash"));
}

#[test]
fn the_helper_refuses_a_release_that_fails_the_pinned_key_and_root_paths_stay_protected() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    let handed = fixture.host.handoffs.borrow().clone();
    let mut next = release("1.0.2");
    let (seed, _) = SigningKey::generate().unwrap();
    let stranger = SigningKey::from_text(&seed).unwrap();
    next.signature = stranger.sign(&next.manifest);
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "manifest_sig");
    assert_eq!(status_file(&fixture).code(), "manifest_sig");

    // The fixture runs the installer and the "service account" as the same
    // user, so ownership alone cannot tell them apart. What can is which paths
    // the installer ever handed to the account: only the account's home,
    // state, credential, driver-home and request directories, and the hub CA.
    let layout = fixture.layout();
    let allowed: std::collections::HashSet<String> = [
        layout.home(),
        layout.state_dir(),
        layout.state_dir().join("credentials"),
        layout.state_dir().join("homes"),
        layout.state_dir().join("update-request"),
        layout.state_dir().join("credentials").join("ca.pem"),
    ]
    .into_iter()
    .map(|path| path.display().to_string())
    .collect();
    assert!(!handed.is_empty(), "the account's own directories are handed over");
    for name in &handed {
        assert!(allowed.contains(name), "installer handed {name} to the service account");
    }
    // Reinstalling re-verifies the same paths rather than skipping the log.
    fixture.install(&release("1.0.1")).unwrap();
    let again = fixture.host.handoffs.borrow().clone();
    assert!(
        again.len() >= handed.len() * 2,
        "re-verification must record handoffs again: first {} then {}",
        handed.len(),
        again.len()
    );

    let layout = fixture.layout();
    let installer_uid = unsafe { libc::geteuid() };
    let current = fixture.at(layout.current());
    assert_eq!(fs::read_link(&current).unwrap(), Path::new("releases/1.0.1"));
    let parent = current.parent().unwrap();
    let parent_meta = fs::metadata(parent).unwrap();
    assert_eq!(parent_meta.uid(), installer_uid, "current's parent is installer-owned");
    assert_eq!(parent_meta.mode() & 0o022, 0, "current's parent is writable by others");
    let release_dir = fixture.at(layout.release_dir("1.0.1"));
    let release_meta = fs::metadata(&release_dir).unwrap();
    assert_eq!(
        release_meta.uid(),
        installer_uid,
        "the release directory is installer-owned"
    );
    assert_eq!(
        release_meta.mode() & 0o022,
        0,
        "release directory is writable by others"
    );
    for path in [
        layout.pinned_key_path(),
        layout.helper_definition(),
        layout.config_path(),
    ] {
        let meta = fs::symlink_metadata(fixture.at(path.clone())).unwrap();
        assert_eq!(meta.uid(), installer_uid, "{} is installer-owned", path.display());
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
        assert!(fixture.at(path.clone()).is_file());
        let path_unit = fs::read_to_string(fixture.at(path)).unwrap();
        assert!(path_unit.contains("PathExists="), "{path_unit}");
        assert!(
            path_unit.contains("upgrade-active")
                && path_unit.contains("request.json")
                && path_unit.contains("update-claimed.json"),
            "{path_unit}"
        );
    } else {
        let plist = fs::read_to_string(fixture.at(fixture.layout().helper_definition())).unwrap();
        assert!(plist.contains("KeepAlive") && plist.contains("PathState"), "{plist}");
        assert!(plist.contains("upgrade-active"), "{plist}");
        assert!(plist.contains("update-claimed.json"), "{plist}");
        assert!(plist.contains("<integer>2700</integer>"), "{plist}");
    }
    if !Platform::host().unwrap().is_macos() {
        let unit = fs::read_to_string(fixture.at(fixture.layout().helper_definition())).unwrap();
        assert!(unit.contains("TimeoutStartSec=2700"), "{unit}");
        assert!(unit.contains("Restart=on-failure"), "{unit}");
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
        fetcher: &MapFetcher { files: HashMap::new() },
        probe: &AlwaysReady,
        health: INSTANT,
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
    })
    .unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "request_malformed");
    assert!(!path.exists());
    assert_eq!(status_file(&fixture).code(), "request_malformed");

    fs::write(&path, b"{\"schema\":1}").unwrap();
    let status = update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher: &MapFetcher { files: HashMap::new() },
        probe: &AlwaysReady,
        health: INSTANT,
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
    })
    .unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "request_malformed");
    assert!(!path.exists());
}

#[test]
fn a_restart_failure_after_switch_rolls_back() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    fixture.seed_workers();
    fixture.host.flags.restart_fails.set(true);
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
        require_handshake: true,
    };
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &probe).unwrap();
    // The stale file cannot vouch for the candidate, and it cannot vouch for
    // the restored release either: rollback is held to the same contract.
    assert_eq!(status.result, "rollback_failed");
    assert_eq!(status.code(), "health_failed");
    assert!(
        status.error.contains("restoring 1.0.1 not confirmed"),
        "{}",
        status.error
    );
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
}

#[test]
fn a_journal_version_committed_only_in_wal_is_visible_to_the_gate() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    let path = fixture.at(fixture.layout().state_dir().join("journal.db"));
    // Main file says 6; the committed WAL frame says 8; the writer is still open.
    let writer = write_journal_wal(&path, 6, 8);
    assert_eq!(header_user_version(&path), 6, "the version must exist only in the WAL");
    assert_eq!(
        update::journal_user_version(&fixture.host, &fixture.layout(), &fixture.host.account, &inspector()).unwrap(),
        Some(8),
        "the gate must read the WAL, not the main-file header"
    );
    let mut next = release("1.0.2");
    let mut parsed: Manifest = serde_json::from_slice(&next.manifest).unwrap();
    parsed.oldest_journal_version = 9;
    next.manifest = parsed.to_bytes();
    next.signature = key().sign(&next.manifest);
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "journal_incompatible");
    assert!(status.error.contains('8'), "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    drop(writer);
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
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_refused"));
    assert!(
        !fixture.at(fixture.layout().request_path()).exists(),
        "an unresolved outcome still retires the request"
    );
    let previous = || fs::read_link(fixture.at(fixture.layout().previous())).unwrap();
    assert_eq!(previous(), Path::new("releases/1.0.1"));

    // The next activation leaves the baseline alone.
    let again = update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher: &MapFetcher { files: HashMap::new() },
        probe: &AlwaysReady,
        health: INSTANT,
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
    })
    .unwrap();
    assert_eq!(again.result, "rollback_refused_journal_migrated");
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_refused"));
    assert_eq!(previous(), Path::new("releases/1.0.1"));

    // A fix-forward release is not turned away as busy, and `previous` stays
    // the last good release rather than the failed candidate.
    let fixed = release("1.0.3");
    let (mut request, fetcher) = offer(&fixed);
    request.command_id = "cmd-2".into();
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "ok", "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.3"));
    assert_eq!(previous(), Path::new("releases/1.0.1"));
    assert!(in_progress_phase(&fixture).is_none());
}

#[test]
fn a_rollback_that_cannot_prove_the_restored_release_healthy_is_reported_and_retired() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    // The restored release never passes the health contract either; bare
    // liveness of its process is not accepted.
    let status = apply(&fixture, &request, &fetcher, &NeverReady).unwrap();
    assert_eq!(status.result, "rollback_failed");
    assert!(status.error.contains("health_failed"), "{}", status.error);
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_failed"));
    assert!(!fixture.at(fixture.layout().request_path()).exists());
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    let previous = || fs::read_link(fixture.at(fixture.layout().previous())).unwrap();
    assert_eq!(previous(), Path::new("releases/1.0.1"));
    // The retry on the next activation can resolve it once the release is healthy.
    let retry = update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher: &MapFetcher { files: HashMap::new() },
        probe: &AlwaysReady,
        health: INSTANT,
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
    })
    .unwrap();
    assert_eq!(retry.result, "idle");
    assert!(in_progress_phase(&fixture).is_none());
    assert_eq!(status_file(&fixture).result, "rolled_back");
}

#[test]
fn an_error_after_the_daemon_was_stopped_restarts_the_unchanged_release() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    // Not a SQLite database, but long enough to pass the size check.
    let journal = fixture.at(fixture.layout().state_dir().join("journal.db"));
    fs::write(&journal, vec![b'x'; 512]).unwrap();
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "failed");
    assert_eq!(*fixture.host.calls.borrow(), ["stop", "restart"]);
    assert!(fixture.host.flags.running.get());
    assert!(status.error.contains("service restarted on 1.0.1"), "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    assert!(!fixture.at(fixture.layout().request_path()).exists());
    assert!(!fixture.at(fixture.layout().in_progress_path()).exists());
}

#[test]
fn a_restart_that_is_not_healthy_after_a_refusal_is_reported_and_stays_recoverable() {
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
    let status = apply(&fixture, &request, &fetcher, &NeverReady).unwrap();
    assert_eq!(status.result, "restore_failed");
    assert!(status.error.contains("journal_incompatible"), "{}", status.error);
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("restore_failed"));
    assert!(!fixture.at(fixture.layout().request_path()).exists());
}

#[test]
fn a_crash_after_the_daemon_was_stopped_is_recovered_by_starting_it_again() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    // What the helper left behind: marker durable, daemon stopped.
    fs::write(
        fixture.at(fixture.layout().in_progress_path()),
        br#"{"from":"1.0.1","to":"1.0.2","phase":"preparing","command_id":"cmd-1","journal_version":0,"nonce":"n"}"#,
    )
    .unwrap();
    assert!(!fixture.host.flags.running.get());
    let status = update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher: &MapFetcher { files: HashMap::new() },
        probe: &AlwaysReady,
        health: INSTANT,
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
    })
    .unwrap();
    assert_eq!(status.result, "idle");
    assert_eq!(*fixture.host.calls.borrow(), ["restart"]);
    assert!(fixture.host.flags.running.get());
    assert!(in_progress_phase(&fixture).is_none());
    assert_eq!(status_file(&fixture).code(), "interrupted");
}

#[test]
fn the_helper_waits_for_the_daemon_to_exit_before_it_reads_the_journal() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    write_journal(&fixture.at(fixture.layout().state_dir().join("journal.db")), 6);
    fixture.host.flags.extra.stop.linger_polls.set(3);
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let health = Health {
        deadline: Duration::from_secs(5),
        poll: Duration::from_millis(1),
        stable: Duration::ZERO,
    };
    update::write_request(&fixture.at(fixture.layout().request_path()), &request).unwrap();
    let status = update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher: &fetcher,
        probe: &AlwaysReady,
        health,
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
    })
    .unwrap();
    assert_eq!(status.result, "ok");
    assert!(
        fixture.host.flags.extra.stop.polls.get() >= 4,
        "the helper must keep polling until the instance is gone, not trust the stop call"
    );
    assert_eq!(*fixture.host.calls.borrow(), ["stop", "restart"]);
}

#[test]
fn a_daemon_that_never_exits_is_not_upgraded_under_and_is_started_again() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    write_journal(&fixture.at(fixture.layout().state_dir().join("journal.db")), 6);
    fixture.host.flags.extra.stop.linger_polls.set(u32::MAX);
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "failed");
    assert!(status.error.contains("stop_failed"), "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    assert!(!fixture.at(fixture.layout().release_dir("1.0.2")).exists());
    assert_eq!(fixture.host.calls.borrow().last(), Some(&"restart"));
}

#[test]
fn test_hooks_are_refused_outside_staged_mode() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    let installer = inspector();
    let config = fixture.at(fixture.layout().config_path());
    for (args, env) in [
        (vec!["--fixture-files", "/nonexistent"], None),
        (vec![], Some(("BOSS_SPOKE_UPDATE_PROBE", "never"))),
        (vec![], Some(("BOSS_SPOKE_UPDATE_CRASH", "after_stage"))),
    ] {
        let mut command = std::process::Command::new(&installer);
        command
            .args(["apply-update", "--config", config.to_str().unwrap()])
            .args(&args);
        if let Some((key, value)) = env {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("test hook"), "{args:?} {env:?}: {stderr}");
    }
}

fn previous(fixture: &Fixture) -> PathBuf {
    fs::read_link(fixture.at(fixture.layout().previous())).unwrap()
}

fn seed_journal_refusal(fixture: &Fixture) -> Status {
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    write_journal(&fixture.at(fixture.layout().state_dir().join("journal.db")), 6);
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let probe = BumpJournal {
        path: fixture.at(fixture.layout().state_dir().join("journal.db")),
    };
    apply(fixture, &request, &fetcher, &probe).unwrap()
}

#[test]
fn a_refused_offer_does_not_clear_an_outstanding_rollback_marker() {
    let fixture = Fixture::new();
    let status = seed_journal_refusal(&fixture);
    assert_eq!(status.result, "rollback_refused_journal_migrated");
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_refused"));
    assert_eq!(previous(&fixture), Path::new("releases/1.0.1"));
    let marker = fs::read(fixture.at(fixture.layout().in_progress_path())).unwrap();

    let next = release("1.0.3");
    let (mut request, mut fetcher) = offer(&next);
    request.command_id = "cmd-bad-sig".into();
    fetcher
        .files
        .insert(format!("{}.sig", request.manifest_url), b"not-a-signature".to_vec());
    let refused = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(refused.result, "refused");
    assert_eq!(refused.unresolved_result, "rollback_refused_journal_migrated");
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_refused"));
    assert_eq!(
        fs::read(fixture.at(fixture.layout().in_progress_path())).unwrap(),
        marker
    );
    assert_eq!(previous(&fixture), Path::new("releases/1.0.1"));

    let (mut request, fetcher) = offer(&next);
    request.command_id = "cmd-origin".into();
    request.manifest_url = "https://evil.example/manifest.json".into();
    let refused = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(refused.code(), "unapproved_origin");
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_refused"));

    let (mut request, _) = offer(&next);
    request.command_id = "cmd-fetch".into();
    let refused = apply(&fixture, &request, &MapFetcher { files: HashMap::new() }, &AlwaysReady).unwrap();
    assert_eq!(refused.result, "refused");
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_refused"));

    let (mut request, fetcher) = offer(&release("1.0.1"));
    request.command_id = "cmd-old".into();
    let refused = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_ne!(refused.result, "ok", "the installed-record shortcut must not fire");
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_refused"));
    assert_eq!(previous(&fixture), Path::new("releases/1.0.1"));

    let (mut request, fetcher) = offer(&release("1.0.3"));
    request.command_id = "cmd-ok".into();
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "ok", "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.3"));
    assert_eq!(previous(&fixture), Path::new("releases/1.0.1"));
}

#[test]
fn a_follow_on_offer_keeps_the_baseline_journal_limit() {
    let fixture = Fixture::new();
    seed_journal_refusal(&fixture);
    assert_eq!(fixture.current(), Path::new("releases/1.0.2"));

    // Refused before switch: restore the running candidate, not baseline A.
    let mut too_new = release("1.0.3");
    let mut parsed: Manifest = serde_json::from_slice(&too_new.manifest).unwrap();
    parsed.oldest_journal_version = 100;
    too_new.manifest = parsed.to_bytes();
    too_new.signature = key().sign(&too_new.manifest);
    let (mut request, fetcher) = offer(&too_new);
    request.command_id = "cmd-pre-switch".into();
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "refused");
    assert_eq!(status.code(), "journal_incompatible");
    assert_eq!(fixture.current(), Path::new("releases/1.0.2"));
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_refused"));
    assert_eq!(previous(&fixture), Path::new("releases/1.0.1"));

    // Health failure after switch: rollback is checked against A's limit (6),
    // not the migrated journal (99).
    let (mut request, fetcher) = offer(&release("1.0.4"));
    request.command_id = "cmd-health".into();
    let status = apply(&fixture, &request, &fetcher, &FailsFor("1.0.4")).unwrap();
    assert_eq!(status.result, "rollback_refused_journal_migrated", "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.4"));
    assert_eq!(previous(&fixture), Path::new("releases/1.0.1"));
}

#[test]
fn rollback_stops_the_candidate_before_it_reads_the_journal() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    let journal = fixture.at(fixture.layout().state_dir().join("journal.db"));
    write_journal(&journal, 6);
    *fixture.host.flags.extra.stop.journal.borrow_mut() = Some((journal, 99));
    fixture.host.flags.extra.stop.schedule.set((0, 2));
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &FailsFor("1.0.2")).unwrap();
    assert_eq!(status.result, "rollback_refused_journal_migrated", "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.2"));
    assert!(status.error.contains("v99"), "{}", status.error);
}

#[test]
fn an_attempt_file_failure_after_switch_rolls_back_and_leaves_the_service_running() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    write_journal(&fixture.at(fixture.layout().state_dir().join("journal.db")), 6);
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    update::write_request(&fixture.at(fixture.layout().request_path()), &request).unwrap();
    let status = update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher: &fetcher,
        probe: &AlwaysReady,
        health: INSTANT,
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: Some("attempt_write".into()),
    })
    .unwrap();
    assert_eq!(status.result, "rolled_back", "{}", status.error);
    assert!(status.error.contains("attempt file"), "{}", status.error);
    assert!(fixture.host.flags.running.get());
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    assert!(in_progress_phase(&fixture).is_none());
}

#[test]
fn a_journal_read_failure_inside_rollback_is_unresolved() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    write_journal(&fixture.at(fixture.layout().state_dir().join("journal.db")), 6);
    fixture.host.flags.extra.inspector_fail_on.set(2);
    let next = release("1.0.2");
    let (request, fetcher) = offer(&next);
    let status = apply(&fixture, &request, &fetcher, &FailsFor("1.0.2")).unwrap();
    assert_eq!(status.result, "rollback_failed", "{}", status.error);
    assert!(status.error.contains("cannot read journal version"), "{}", status.error);
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_failed"));
    assert!(fixture.host.flags.running.get(), "the spoke must be running");
}

// ---------------------------------------------------------------------------
// Real daemon plus isolated hub. The helper's stop/restart signals only the
// daemon PID (what `KillMode=process` does). Health requires a management
// handshake against the hub fixture. A worker is forked as a descendant of
// the first daemon process and must survive SIGTERM of that PID alone.
// ---------------------------------------------------------------------------

struct DaemonHost {
    account: Account,
    layout: Layout,
    child: RefCell<Option<std::process::Child>>,
    runs: Cell<u64>,
    live: DaemonLive,
}

struct DaemonLive {
    calls: RefCell<Vec<&'static str>>,
    hub: String,
    worker_pid: Cell<Option<u32>>,
}

impl DaemonHost {
    fn new(fixture: &Fixture, hub: String) -> Self {
        Self {
            account: fixture.host.account,
            layout: fixture.layout(),
            child: RefCell::new(None),
            runs: Cell::new(0),
            live: DaemonLive {
                calls: RefCell::default(),
                hub,
                worker_pid: Cell::new(None),
            },
        }
    }

    fn at(&self, logical: PathBuf) -> PathBuf {
        self.layout.on_disk(&logical)
    }

    fn signal_and_reap(&self) {
        if let Some(mut child) = self.child.borrow_mut().take() {
            // SAFETY: the pid is our own live child.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let _ = child.wait();
        }
    }

    fn pid(&self) -> Option<u32> {
        self.child.borrow().as_ref().map(std::process::Child::id)
    }

    fn root(&self) -> &Path {
        self.layout.root.as_deref().expect("staged layout")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root().join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for DaemonHost {
    fn drop(&mut self) {
        self.signal_and_reap();
    }
}

impl Host for DaemonHost {
    fn lookup_user(&self, _: &str) -> Result<Option<Account>> {
        Ok(Some(self.account))
    }
    fn create_user(&self, _: &str, _: &Path) -> Result<Account> {
        Ok(self.account)
    }
    fn register_service(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn service_registered(&self) -> Result<bool> {
        Ok(true)
    }
    fn restart_service(&self) -> Result<()> {
        self.live.calls.borrow_mut().push("restart");
        self.signal_and_reap();
        let binary = self.at(self.layout.current()).join("bin/boss-spoke");
        let state = self.at(self.layout.state_dir());
        let tls = state.join("credentials");
        let tmux = tmux_bin();
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root().join("daemon.log"))?;
        let args = [
            "--state-dir",
            state.to_str().unwrap(),
            "--host-id",
            "host",
            "--hub",
            self.live.hub.as_str(),
            "--tls-dir",
            tls.to_str().unwrap(),
            "--cube-bin",
            "/nonexistent/cube",
            "--tmux-bin",
            tmux.to_str().unwrap(),
        ];
        let first = self.runs.get() == 0;
        let mut command = if first {
            let wrapper = self.root().join("daemon-wrap.sh");
            let pid_file = self.root().join("worker.pid");
            fs::write(
                &wrapper,
                format!(
                    "#!/bin/sh\nsleep 3600 &\necho $! > {}\nexec {} \"$@\"\n",
                    pid_file.display(),
                    binary.display()
                ),
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
            }
            let mut command = std::process::Command::new(&wrapper);
            command.args(args);
            command
        } else {
            let mut command = std::process::Command::new(&binary);
            command.args(args);
            command
        };
        let child = command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root())
            .env("BOSS_SPOKE_INSTALL_CONFIG", self.at(self.layout.config_path()))
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .map_err(|error| anyhow::anyhow!("cannot start {}: {error}", binary.display()))?;
        self.runs.set(self.runs.get() + 1);
        if first {
            let pid_file = self.root().join("worker.pid");
            // The wrapper only has to fork `sleep`, but a loaded CI host can
            // take far longer than a second to schedule it.
            for _ in 0..1500 {
                if let Ok(text) = fs::read_to_string(&pid_file)
                    && let Ok(pid) = text.trim().parse::<u32>()
                {
                    self.live.worker_pid.set(Some(pid));
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        *self.child.borrow_mut() = Some(child);
        Ok(())
    }
    fn stop_service(&self) -> Result<()> {
        self.live.calls.borrow_mut().push("stop");
        self.signal_and_reap();
        Ok(())
    }
    fn service_running(&self) -> Result<bool> {
        Ok(self.service_instance()?.is_some())
    }
    fn service_instance(&self) -> Result<Option<(u32, u64)>> {
        let mut child = self.child.borrow_mut();
        Ok(match child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(None) => Some((child.id(), self.runs.get())),
                _ => None,
            },
            None => None,
        })
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

/// The real binary, once: it is large and every release here carries it.
fn real_spoke_bytes() -> &'static [u8] {
    static BYTES: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    BYTES.get_or_init(|| fs::read(fs::canonicalize(std::env::var("SPOKE_BIN").unwrap()).unwrap()).unwrap())
}

fn real_daemon_release(id: &str, daemon: Vec<u8>) -> Release {
    let built = build(
        daemon,
        real("INSTALLER_BIN", id),
        id,
        Platform::host().unwrap(),
        URL_BASE,
    )
    .unwrap();
    let manifest = built.manifest.to_bytes();
    Release {
        signature: key().sign(&manifest),
        manifest,
        package: built.package,
    }
}

fn working_release(id: &str) -> Release {
    real_daemon_release(id, real_spoke_bytes().to_vec())
}

/// A candidate whose daemon exits at once.
fn dying_release(id: &str) -> Release {
    real_daemon_release(id, b"#!/bin/sh\nexit 1\n".to_vec())
}

struct Live {
    fixture: Fixture,
    host: DaemonHost,
    _rt: tokio::runtime::Runtime,
    _hub: boss_spoke_test_hub::Fixture,
}

impl Live {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = rt.block_on(async {
            let mut hub = boss_spoke_test_hub::Fixture::new().await;
            hub.auto_welcome();
            hub
        });
        let fixture = Fixture::new();
        let host = DaemonHost::new(&fixture, hub.endpoint.clone());
        Self {
            fixture,
            host,
            _rt: rt,
            _hub: hub,
        }
    }

    fn enroll(&self) {
        let credentials = self.fixture.at(self.fixture.layout().state_dir().join("credentials"));
        fs::copy(self._hub.ca_pem(), credentials.join("ca.pem")).unwrap();
        fs::copy(self._hub.cert_pem(), credentials.join("cert.pem")).unwrap();
        fs::copy(self._hub.key_pem(), credentials.join("key.pem")).unwrap();
    }

    fn probe(&self) -> FileProbe<'static> {
        // The layout is leaked: a probe borrowing it must outlive nothing here.
        let layout: &'static Layout = Box::leak(Box::new(self.fixture.layout()));
        FileProbe {
            layout,
            account: self.fixture.host.account,
            require_handshake: true,
        }
    }

    fn health(&self) -> serde_json::Value {
        let path = self.fixture.at(self.fixture.layout().health_path());
        serde_json::from_slice(&fs::read(path).unwrap_or_default()).unwrap_or(serde_json::Value::Null)
    }

    /// Starts the daemon that `current` names and waits until it reports
    /// itself ready as `release`.
    fn start(&self, release: &str) {
        self.host.restart_service().unwrap();
        let pid = self.host.pid().unwrap();
        self.wait_ready(release, pid);
        self.host.live.calls.borrow_mut().clear();
    }

    fn wait_ready(&self, release: &str, pid: u32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let health = self.health();
            let flag = |key: &str| health.get(key).and_then(serde_json::Value::as_bool) == Some(true);
            if flag("lock_ok")
                && flag("journal_ok")
                && flag("inventory_ok")
                && flag("handshake_ok")
                && health["release_id"] == release
                && health["pid"] == pid
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline && self.host.service_instance().unwrap().is_some(),
                "the daemon did not report ready as {release}: health {health}\n{}",
                self.host.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn helper_health() -> Health {
        Health {
            deadline: Duration::from_secs(60),
            poll: Duration::from_millis(100),
            stable: Duration::from_millis(300),
        }
    }

    fn upgrade(&self, release: &Release, deadline: Duration) -> Status {
        let (request, fetcher) = offer(release);
        update::write_request(&self.fixture.at(self.fixture.layout().request_path()), &request).unwrap();
        let probe = self.probe();
        update::apply(&Apply {
            host: &self.host,
            layout: &self.fixture.layout(),
            fetcher: &fetcher,
            probe: &probe,
            health: Health {
                deadline,
                ..Self::helper_health()
            },
            inspector: &inspector(),
            crash: None,
            unwind_crash: false,
            fail: None,
        })
        .unwrap()
    }

    fn worker_alive(&self) -> bool {
        let Some(pid) = self.host.live.worker_pid.get() else {
            return false;
        };
        // SAFETY: signal 0 only checks that the pid exists.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn journal_version(&self) -> Option<u32> {
        update::journal_user_version(
            &self.host,
            &self.fixture.layout(),
            &self.fixture.host.account,
            &inspector(),
        )
        .unwrap()
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(pid) = self.host.live.worker_pid.get() {
            // SAFETY: best-effort cleanup of the descendant worker.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
}

#[test]
fn a_real_daemon_is_upgraded_in_place_and_its_worker_and_journal_survive() {
    let live = Live::new();
    live.fixture.install(&working_release("1.0.1")).unwrap();
    live.enroll();
    live.start("1.0.1");
    let before = live.host.pid().unwrap();
    let version = live.journal_version();
    assert!(
        version.is_some_and(|v| v > 0),
        "the daemon creates its own journal: {version:?}"
    );

    let status = live.upgrade(&working_release("1.0.2"), Duration::from_secs(60));
    assert_eq!(status.result, "ok", "{}\n{}", status.error, live.host.log());
    assert_eq!(live.fixture.current(), Path::new("releases/1.0.2"));
    assert_eq!(*live.host.live.calls.borrow(), ["stop", "restart"]);
    let after = live.host.pid().unwrap();
    assert_ne!(before, after, "the daemon must have been replaced");
    // The new process says it runs 1.0.2 because that is where its executable
    // lives, not because anyone told it so.
    let health = live.health();
    assert_eq!(health["release_id"], "1.0.2");
    assert_eq!(health["pid"], after);
    assert_eq!(live.journal_version(), version, "the new daemon reads the same journal");
    assert!(live.worker_alive(), "a worker must survive the switch and restart");
}

#[test]
fn a_candidate_that_dies_is_rolled_back_onto_a_daemon_that_is_really_healthy() {
    let live = Live::new();
    live.fixture.install(&working_release("1.0.1")).unwrap();
    live.enroll();
    live.start("1.0.1");
    let version = live.journal_version();

    let status = live.upgrade(&dying_release("1.0.2"), Duration::from_secs(3));
    assert_eq!(status.result, "rolled_back", "{}\n{}", status.error, live.host.log());
    assert_eq!(live.fixture.current(), Path::new("releases/1.0.1"));
    let pid = live.host.pid().expect("the restored daemon is running");
    live.wait_ready("1.0.1", pid);
    assert_eq!(live.journal_version(), version);
    assert!(live.worker_alive(), "a worker must survive rollback");
    assert!(!live.fixture.at(live.fixture.layout().in_progress_path()).exists());
    assert!(!live.fixture.at(live.fixture.layout().request_path()).exists());
}

#[test]
fn a_refusal_leaves_the_real_daemon_running_on_the_same_release() {
    let live = Live::new();
    live.fixture.install(&working_release("1.0.1")).unwrap();
    live.enroll();
    live.start("1.0.1");
    let mut next = working_release("1.0.2");
    let mut parsed: Manifest = serde_json::from_slice(&next.manifest).unwrap();
    parsed.oldest_journal_version = 9999;
    next.manifest = parsed.to_bytes();
    next.signature = key().sign(&next.manifest);

    let status = live.upgrade(&next, Duration::from_secs(60));
    assert_eq!(status.result, "refused", "{}\n{}", status.error, live.host.log());
    assert_eq!(status.code(), "journal_incompatible");
    assert_eq!(*live.host.live.calls.borrow(), ["stop", "restart"]);
    let pid = live.host.pid().expect("the spoke must be running after a refusal");
    live.wait_ready("1.0.1", pid);
    assert_eq!(live.fixture.current(), Path::new("releases/1.0.1"));
    assert!(live.worker_alive());
    assert!(!live.fixture.at(live.fixture.layout().in_progress_path()).exists());
}

#[test]
fn a_crash_between_stop_and_the_first_switch_is_recovered_with_a_real_daemon() {
    let live = Live::new();
    live.fixture.install(&working_release("1.0.1")).unwrap();
    live.enroll();
    live.start("1.0.1");
    // The helper stopped the daemon and died: marker durable, nothing running.
    fs::write(
        live.fixture.at(live.fixture.layout().in_progress_path()),
        br#"{"from":"1.0.1","to":"1.0.2","phase":"preparing","command_id":"cmd-1","journal_version":0,"nonce":"n"}"#,
    )
    .unwrap();
    live.host.stop_service().unwrap();
    assert!(live.host.service_instance().unwrap().is_none());
    live.host.live.calls.borrow_mut().clear();

    let probe = live.probe();
    let status = update::apply(&Apply {
        host: &live.host,
        layout: &live.fixture.layout(),
        fetcher: &MapFetcher { files: HashMap::new() },
        probe: &probe,
        health: Live::helper_health(),
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
    })
    .unwrap();
    assert_eq!(status.result, "idle");
    assert_eq!(*live.host.live.calls.borrow(), ["restart"]);
    let pid = live.host.pid().expect("recovery restarts the daemon");
    live.wait_ready("1.0.1", pid);
    assert!(live.worker_alive());
}

#[test]
fn a_follow_on_offer_interrupted_before_switch_keeps_the_baseline() {
    let fixture = Fixture::new();
    seed_journal_refusal(&fixture);
    let next = release("1.0.3");
    let (mut request, fetcher) = offer(&next);
    request.command_id = "cmd-interrupt".into();
    update::write_request(&fixture.at(fixture.layout().request_path()), &request).unwrap();
    let crashed = update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher: &fetcher,
        probe: &AlwaysReady,
        health: INSTANT,
        inspector: &inspector(),
        crash: Some("after_prepare_marker".into()),
        unwind_crash: true,
        fail: None,
    });
    assert!(crashed.is_err(), "{crashed:?}");
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("preparing"));
    let recovered = update::apply(&Apply {
        host: &fixture.host,
        layout: &fixture.layout(),
        fetcher: &fetcher,
        probe: &AlwaysReady,
        health: INSTANT,
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
    })
    .unwrap();
    assert_eq!(
        recovered.result, "rollback_refused_journal_migrated",
        "{}",
        recovered.result
    );
    assert_eq!(previous(&fixture), Path::new("releases/1.0.1"));
    assert_eq!(in_progress_phase(&fixture).as_deref(), Some("rollback_refused"));
    assert_eq!(fixture.current(), Path::new("releases/1.0.2"));

    let (mut request, fetcher) = offer(&release("1.0.3"));
    request.command_id = "cmd-after-interrupt".into();
    let status = apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap();
    assert_eq!(status.result, "ok", "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.3"));
    assert_eq!(previous(&fixture), Path::new("releases/1.0.1"));
}

#[test]
fn real_daemon_crashes_at_each_step_recover_healthy() {
    for point in ["after_prepare_marker", "after_stop", "after_stage", "after_switch"] {
        let live = Live::new();
        live.fixture.install(&working_release("1.0.1")).unwrap();
        live.enroll();
        live.start("1.0.1");
        let version = live.journal_version();
        let next = working_release("1.0.2");
        let (request, fetcher) = offer(&next);
        update::write_request(&live.fixture.at(live.fixture.layout().request_path()), &request).unwrap();
        let probe = live.probe();
        let crashed = update::apply(&Apply {
            host: &live.host,
            layout: &live.fixture.layout(),
            fetcher: &fetcher,
            probe: &probe,
            health: Live::helper_health(),
            inspector: &inspector(),
            crash: Some(point.into()),
            unwind_crash: true,
            fail: None,
        });
        assert!(crashed.is_err(), "{point}: {crashed:?}");
        let recovered = update::apply(&Apply {
            host: &live.host,
            layout: &live.fixture.layout(),
            fetcher: &fetcher,
            probe: &probe,
            health: Live::helper_health(),
            inspector: &inspector(),
            crash: None,
            unwind_crash: false,
            fail: None,
        })
        .unwrap();
        assert!(
            recovered.result == "ok" || recovered.result == "idle" || recovered.result == "rolled_back",
            "{point}: {} {}",
            recovered.result,
            recovered.error
        );
        let pid = live.host.pid().unwrap_or_else(|| panic!("{point}: daemon running"));
        let release = if live.fixture.current() == Path::new("releases/1.0.2") {
            "1.0.2"
        } else {
            "1.0.1"
        };
        live.wait_ready(release, pid);
        assert_eq!(live.journal_version(), version, "{point}: journal continuity");
        assert!(live.worker_alive(), "{point}: worker must survive");
    }
}

#[test]
fn a_worker_tmux_session_survives_a_real_daemon_upgrade() {
    if !tmux_is_usable() {
        assert!(
            std::env::var_os("BOSS_REQUIRE_TMUX").is_none(),
            "tmux is required in this environment and is not usable"
        );
        return;
    }
    let live = Live::new();
    live.fixture.install(&working_release("1.0.1")).unwrap();
    live.enroll();
    live.start("1.0.1");
    let socket = live.fixture.at(live.fixture.layout().state_dir().join("tmux.sock"));
    let tmux = |args: &[&str]| {
        let out = std::process::Command::new(tmux_bin())
            .args(["-S", socket.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        )
    };
    assert!(tmux(&["new-session", "-d", "-s", "worker", "sleep", "3600"]).0);
    assert!(tmux(&["set-environment", "-t", "worker", "BOSS_MARK", "survivor"]).0);
    let pane = tmux(&["display-message", "-p", "-t", "worker", "#{pane_pid}"]).1;

    let status = live.upgrade(&working_release("1.0.2"), Duration::from_secs(60));
    assert_eq!(status.result, "ok", "{}\n{}", status.error, live.host.log());
    assert_eq!(tmux(&["display-message", "-p", "-t", "worker", "#{pane_pid}"]).1, pane);
    assert_eq!(
        tmux(&["show-environment", "-t", "worker", "BOSS_MARK"]).1,
        "BOSS_MARK=survivor"
    );
    let _ = tmux(&["kill-server"]);
}

#[test]
fn an_idle_run_removes_a_stale_active_flag() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    let layout = fixture.layout();
    fs::write(fixture.at(layout.active_path()), b"").unwrap();
    let status = update::apply(&Apply {
        host: &fixture.host,
        layout: &layout,
        fetcher: &MapFetcher { files: HashMap::new() },
        probe: &AlwaysReady,
        health: INSTANT,
        inspector: &inspector(),
        crash: None,
        unwind_crash: false,
        fail: None,
    })
    .unwrap();
    assert_eq!(status.result, "idle");
    assert!(!fixture.at(layout.active_path()).exists());
}

#[test]
fn an_error_before_the_marker_clears_the_claim_and_active_flag() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.host.flags.user_exists.set(false);
    let (request, fetcher) = offer(&release("1.0.2"));
    assert!(apply(&fixture, &request, &fetcher, &AlwaysReady).is_err());
    assert!(!fixture.at(fixture.layout().active_path()).exists());
    assert!(!fixture.at(fixture.layout().claimed_path()).exists());
}

#[test]
fn recovery_helper_stays_on_the_committed_release_until_health_succeeds() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    let helper = fixture.at(fixture.layout().prefix().join("update-helper"));
    let original = fs::read(&helper).unwrap();
    let definition = fs::read_to_string(fixture.at(fixture.layout().helper_definition())).unwrap();
    assert!(definition.contains("/update-helper"));
    assert!(!definition.contains("/current/bin/boss-spoke-package"));
    let (request, fetcher) = offer(&release("1.0.2"));
    write_request_and_map(&fixture, &request, &fetcher);
    let crashed = helper_command(&fixture)
        .env("BOSS_SPOKE_UPDATE_CRASH", "after_switch")
        .output()
        .unwrap();
    assert!(!crashed.status.success());
    assert_eq!(fs::read(&helper).unwrap(), original);
    // Recovery runs the stable entrypoint even if the candidate installer cannot run.
    fs::write(
        fixture.at(fixture.layout().current().join("bin/boss-spoke-package")),
        b"broken",
    )
    .unwrap();
    let mut recovery = helper_command(&fixture);
    recovery = {
        let mut stable = std::process::Command::new(&helper);
        stable.args(recovery.get_args());
        stable
    };
    let output = recovery.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
    assert_eq!(fs::read(&helper).unwrap(), original);
    let (request, fetcher) = offer(&release("1.0.3"));
    assert_eq!(apply(&fixture, &request, &fetcher, &AlwaysReady).unwrap().result, "ok");
    assert_ne!(fs::read(&helper).unwrap(), original);
}

#[test]
fn sigterm_during_candidate_health_finishes_rollback() {
    let fixture = Fixture::new();
    fixture.install(&release("1.0.1")).unwrap();
    fixture.enroll();
    let (request, fetcher) = offer(&release("1.0.2"));
    write_request_and_map(&fixture, &request, &fetcher);
    let mut child = helper_command(&fixture)
        .env("BOSS_SPOKE_UPDATE_PROBE", "only_installed")
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if in_progress_phase(&fixture).as_deref() == Some("switched") {
            // SAFETY: this is the helper child owned by this test.
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
            break;
        }
        assert!(child.try_wait().unwrap().is_none(), "helper exited before signal");
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(child.wait().unwrap().success());
    let status = status_file(&fixture);
    assert_eq!(status.result, "rolled_back", "{}", status.error);
    assert!(status.error.contains("terminated"), "{}", status.error);
    assert_eq!(fixture.current(), Path::new("releases/1.0.1"));
}
