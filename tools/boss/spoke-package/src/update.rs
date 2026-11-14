//! Privileged update helper: the service user names a release; root fetches,
//! verifies, switches and rolls back.
//!
//! Nothing the service user writes is trusted beyond "which HTTPS release to
//! try". The helper re-fetches the manifest, signature and package from the
//! origin pinned at provisioning, and verifies them against the root-pinned
//! release key before it touches `current`.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::install::{
    self, Account, Health, Host, InstallRecord, Layout, claim_request, clear_active, clear_claimed, clear_in_progress,
    current_release, install_release, journal_sqlite_path, point_current, point_previous, read_service_file,
    resolve_pinned_key, unlink_service_file, write_active, write_in_progress, write_logical, write_status,
};
use crate::manifest::{self, Error as ManifestError, Platform, TrustedKey};

pub const REQUEST_SCHEMA: u32 = 1;
pub const STATUS_SCHEMA: u32 = 1;
const FETCH_MAX_BYTES: usize = 64 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const REDIRECT_HOPS: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Request {
    pub schema: u32,
    pub command_id: String,
    pub release_id: String,
    pub manifest_url: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct Status {
    pub schema: u32,
    pub command_id: String,
    pub release_id: String,
    pub result: String,
    pub error: String,
    pub running_release_id: String,
    /// Last unresolved outcome the hub still needs to see. A later refused
    /// offer must not hide a rollback_refused / rollback_failed / restore_failed.
    #[serde(default)]
    #[builder(default)]
    pub unresolved_result: String,
    #[serde(default)]
    #[builder(default)]
    pub unresolved_error: String,
    #[serde(default)]
    #[builder(default)]
    pub unresolved_release_id: String,
}

impl Status {
    pub fn code(&self) -> &str {
        if self.result == "ok" {
            "ok"
        } else if self.error.is_empty() {
            self.result.as_str()
        } else {
            self.error.split(':').next().unwrap_or(self.result.as_str())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
struct InProgress {
    from: String,
    to: String,
    phase: String,
    #[serde(default)]
    #[builder(default)]
    command_id: String,
    #[serde(default)]
    #[builder(default)]
    journal_version: u32,
    #[serde(default)]
    #[builder(default)]
    nonce: String,
    /// The release that was actually running when this attempt started. Restore
    /// restarts this, which may differ from `from` after a journal-migration
    /// refusal left the failed candidate in place.
    #[serde(default)]
    #[builder(default)]
    running: String,
    /// Unresolved phase a follow-on attempt must restore if it never commits.
    /// Empty on a first attempt.
    #[serde(default)]
    #[builder(default)]
    held_phase: String,
}

impl InProgress {
    fn with_held(mut self, held: &str) -> Self {
        self.held_phase = held.to_owned();
        self
    }
}

pub trait Fetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>>;
}

pub struct MapFetcher {
    pub files: HashMap<String, Vec<u8>>,
}

impl Fetcher for MapFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.files
            .get(url)
            .cloned()
            .with_context(|| format!("fetch missed {url}"))
    }
}

/// HTTPS fetch through the process-wide rustls client, with retries and an
/// origin pin on every redirect hop. One runtime serves every fetch of a helper
/// run, so pooled connections never outlive the runtime that spawned them.
pub struct HttpFetcher {
    origin: String,
    runtime: tokio::runtime::Runtime,
}

impl HttpFetcher {
    pub fn new(origin: impl Into<String>) -> Result<Self> {
        Ok(Self {
            origin: origin.into(),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("cannot start fetch runtime")?,
        })
    }
}

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.runtime.block_on(fetch_https(&self.origin, url))
    }
}

/// Refuses a response that declares a body over the cap before reading any of it.
fn check_declared_length(length: Option<u64>) -> Result<()> {
    if let Some(length) = length {
        ensure!(
            length <= FETCH_MAX_BYTES as u64,
            "fetch exceeded {FETCH_MAX_BYTES} bytes (declared {length})"
        );
    }
    Ok(())
}

/// Appends a body chunk, failing as soon as the total would pass the cap.
fn accept_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<()> {
    ensure!(
        body.len().saturating_add(chunk.len()) <= FETCH_MAX_BYTES,
        "fetch exceeded {FETCH_MAX_BYTES} bytes"
    );
    body.extend_from_slice(chunk);
    Ok(())
}

async fn fetch_https(origin: &str, start: &str) -> Result<Vec<u8>> {
    manifest::require_origin(start, origin)?;
    let policy = boss_http_retry::RetryPolicy::new(4, Duration::from_millis(250), Duration::from_secs(30));
    let mut url = start.to_owned();
    for _hop in 0..=REDIRECT_HOPS {
        let mut last_error = None;
        for attempt in 1..=policy.max_attempts {
            check_interrupt()?;
            let response = boss_http_retry::http_client()
                .get(&url)
                .timeout(FETCH_TIMEOUT)
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_redirection() => {
                    let location = response
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|value| value.to_str().ok())
                        .context("redirect without Location")?;
                    url = manifest::redirect_hop(&url, location, origin)?.to_string();
                    last_error = None;
                    break;
                }
                Ok(mut response) if response.status().is_success() => {
                    check_declared_length(response.content_length())?;
                    let mut body = Vec::new();
                    while let Some(chunk) = response.chunk().await.context("body")? {
                        accept_chunk(&mut body, &chunk)?;
                    }
                    return Ok(body);
                }
                Ok(response) if response.status().is_server_error() && attempt < policy.max_attempts => {
                    last_error = Some(format!("HTTP {}", response.status()));
                }
                Ok(response) => bail!("fetch failed for {url}: HTTP {}", response.status()),
                Err(error) if attempt < policy.max_attempts => last_error = Some(error.to_string()),
                Err(error) => return Err(error).context(format!("fetch failed for {url}")),
            }
            if last_error.is_some() {
                let delay = boss_http_retry::jitter(boss_http_retry::backoff_delay(&policy, attempt));
                tokio::time::sleep(delay).await;
            }
        }
        if last_error.is_none() {
            continue;
        }
        bail!("fetch failed for {url}: {}", last_error.unwrap_or_default());
    }
    bail!("too many redirects fetching {start}")
}

pub trait HealthProbe {
    fn ready(&self, release_id: &str, pid: u32, nonce: &str) -> Result<bool>;
}

pub struct AlwaysReady;

impl HealthProbe for AlwaysReady {
    fn ready(&self, _: &str, _: u32, _: &str) -> Result<bool> {
        Ok(true)
    }
}

pub struct NeverReady;

impl HealthProbe for NeverReady {
    fn ready(&self, _: &str, _: u32, _: &str) -> Result<bool> {
        Ok(false)
    }
}

/// Ready only for one release: the installed one. Staged crash runs use it to
/// make a candidate fail its health check while the restored release passes.
pub struct OnlyReady(pub String);

impl HealthProbe for OnlyReady {
    fn ready(&self, release_id: &str, _: u32, _: &str) -> Result<bool> {
        Ok(release_id == self.0)
    }
}

/// True once the new daemon has locked, opened the journal, inventoried and
/// completed a management handshake for this attempt.
pub struct FileProbe<'a> {
    pub layout: &'a Layout,
    pub account: Account,
    /// Production always requires the hub handshake. Only a test with no hub to
    /// connect to turns it off; every other check still applies.
    pub require_handshake: bool,
}

impl HealthProbe for FileProbe<'_> {
    fn ready(&self, release_id: &str, pid: u32, nonce: &str) -> Result<bool> {
        let Some(bytes) = read_service_file(self.layout, &self.layout.health_path(), &self.account)? else {
            return Ok(false);
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        let flag = |key: &str| value.get(key).and_then(|v| v.as_bool()) == Some(true);
        Ok(flag("lock_ok")
            && flag("journal_ok")
            && flag("inventory_ok")
            && (flag("handshake_ok") || !self.require_handshake)
            && value.get("release_id").and_then(|v| v.as_str()) == Some(release_id)
            && value.get("nonce").and_then(|v| v.as_str()) == Some(nonce)
            && value.get("pid").and_then(|v| v.as_u64()) == Some(u64::from(pid)))
    }
}

#[derive(bon::Builder)]
pub struct Apply<'a> {
    pub host: &'a dyn Host,
    pub layout: &'a Layout,
    pub fetcher: &'a dyn Fetcher,
    pub probe: &'a dyn HealthProbe,
    pub health: Health,
    /// Installer binary the journal gate runs as the service user, so root
    /// never opens a database the service user controls.
    pub inspector: &'a Path,
    /// Aborts the process at the named point. Set only by staged test runs.
    pub crash: Option<String>,
    /// When set with `crash`, return an error instead of aborting so in-process
    /// tests can recover without the helper's error-cleanup path running.
    pub unwind_crash: bool,
    /// Named fault for tests (`attempt_write`, `journal_read`).
    pub fail: Option<String>,
}

#[derive(Clone, Copy)]
struct SwitchIds<'a> {
    from: &'a str,
    to: &'a str,
    command_id: &'a str,
    journal_version: u32,
    running: &'a str,
}

/// What an earlier, unfinished attempt left that a new request must respect.
struct Outstanding {
    /// The release every rollback and the `previous` link are anchored to.
    baseline: String,
    /// Journal version that `baseline` can read. Carried unchanged through
    /// later attempts so a follow-on offer cannot re-anchor the rollback gate.
    journal_version: u32,
    /// Release that was live when the unresolved attempt started.
    running: String,
    status: Status,
}

/// Set by SIGTERM so a timed-out helper restores or rolls back instead of
/// exiting with the daemon stopped.
static TERMINATE: AtomicBool = AtomicBool::new(false);

thread_local! {
    static STARTED: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Below `TimeoutStartSec=2700` so SIGTERM still has `TimeoutStopSec` to restore.
const HELPER_BUDGET: Duration = Duration::from_secs(2400);

/// Arm the helper to treat SIGTERM as a restore/rollback request.
pub fn request_terminate() {
    TERMINATE.store(true, Ordering::SeqCst);
}

fn begin_helper_run() {
    TERMINATE.store(false, Ordering::SeqCst);
    STARTED.with(|started| started.set(Some(Instant::now())));
}

#[derive(Clone, Copy)]
enum Cancellation {
    Forward,
    // Two bounded waits (stop and health) fit within the 120-second stop timeout.
    // Forward cancellation cannot consume this recovery allowance.
    Revert(Instant),
}

impl Cancellation {
    fn check(self) -> Result<()> {
        match self {
            Self::Forward => check_interrupt(),
            Self::Revert(started) => {
                ensure!(started.elapsed() < Duration::from_secs(45), "revert deadline exhausted");
                Ok(())
            }
        }
    }
}

fn check_interrupt() -> Result<()> {
    ensure!(!TERMINATE.load(Ordering::SeqCst), "terminated");
    STARTED.with(|started| {
        if let Some(started) = started.get() {
            ensure!(started.elapsed() < HELPER_BUDGET, "terminated: helper budget exhausted");
        }
        Ok(())
    })
}

pub fn apply(ctx: &Apply<'_>) -> Result<Status> {
    begin_helper_run();
    let outstanding = recover(ctx)?;
    let request = match load_request(ctx) {
        Ok(Some(request)) => request,
        Ok(None) => {
            clear_active(ctx.layout)?;
            return Ok(outstanding.map_or_else(
                || status_for(ctx.layout, "", "", "idle", ""),
                |outstanding| outstanding.status,
            ));
        }
        Err(error) => {
            let status = status_for(ctx.layout, "", "", "refused", &format!("request_malformed: {error:#}"));
            return refuse_without_clearing(ctx, status, outstanding.as_ref());
        }
    };
    match apply_request(ctx, &request, outstanding.as_ref()) {
        Ok(status) => Ok(status),
        Err(error) if injected_crash(&error) => Err(error),
        Err(error) => {
            if let Some(status) = recover_after_error(ctx, &request, &error)? {
                return Ok(status);
            }
            let status = status_for(
                ctx.layout,
                &request.command_id,
                &request.release_id,
                "failed",
                &format!("{error:#}"),
            );
            let _ = write_status_file(ctx.layout, &status);
            if !marker_blocks_cleanup(ctx.layout) {
                remove_request(ctx.layout)?;
                if read_marker(ctx.layout).is_none() {
                    clear_active(ctx.layout)?;
                }
            }
            Err(error)
        }
    }
}

fn injected_crash(error: &anyhow::Error) -> bool {
    error.to_string().contains("injected crash at")
}

fn marker_blocks_cleanup(layout: &Layout) -> bool {
    read_marker(layout).is_some_and(|marker| {
        matches!(
            marker.phase.as_str(),
            "preparing" | "staged" | "switched" | "committing" | "rolling_back" | "rollback_quiescing" | "restoring"
        )
    })
}

/// After the daemon has been stopped, an unexpected error must restore or
/// roll back rather than leave the host offline.
fn recover_after_error(ctx: &Apply<'_>, request: &Request, error: &anyhow::Error) -> Result<Option<Status>> {
    let Some(marker) = read_marker(ctx.layout) else {
        return Ok(None);
    };
    let reason = format!("{error:#}");
    match marker.phase.as_str() {
        "switched" | "committing" | "rolling_back" | "rollback_quiescing" => {
            Ok(Some(rollback(ctx, SwitchIds::of(&marker), &marker.nonce, &reason)?))
        }
        "preparing" | "staged" | "restoring" => {
            let outcome = status_for(ctx.layout, &request.command_id, &request.release_id, "failed", &reason);
            Ok(Some(restore_service(ctx, SwitchIds::of(&marker), outcome)?))
        }
        _ => Ok(None),
    }
}

/// The release being installed and everything needed to put the host back.
#[derive(bon::Builder)]
struct Candidate<'a> {
    request: &'a Request,
    account: Account,
    oldest_journal_version: u32,
    package_sha256: &'a str,
    package: &'a [u8],
    from: String,
    to: String,
    nonce: String,
    enrolled: bool,
    running: String,
    baseline_journal: u32,
    held_phase: String,
}

enum Gate {
    Switched { journal_version: u32 },
    Refused(Status),
}

fn apply_request(ctx: &Apply<'_>, request: &Request, outstanding: Option<&Outstanding>) -> Result<Status> {
    let record = InstallRecord::read(&ctx.layout.on_disk(&ctx.layout.config_path()))?;
    ensure!(
        record.schema == 1,
        "unsupported install record schema {}",
        record.schema
    );
    ensure!(
        !record.package_origin.is_empty(),
        "this host has no pinned package origin; reinstall the signed package"
    );
    let account = service_account(ctx, &record)?;
    if request.schema != REQUEST_SCHEMA
        || request.command_id.is_empty()
        || request.release_id.is_empty()
        || !manifest::is_token(&request.release_id, 64)
        || request.manifest_sha256.len() != 64
        || !request
            .manifest_sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return refuse_without_clearing(
            ctx,
            refused(ctx.layout, request, "manifest_malformed: request is not a usable offer"),
            outstanding,
        );
    }
    if let Err(error) = manifest::require_origin(&request.manifest_url, &record.package_origin) {
        return refuse_without_clearing(ctx, refused(ctx.layout, request, &error.to_string()), outstanding);
    }
    let current = current_release(ctx.layout);
    if outstanding.is_none() && current.as_deref() == Some(request.release_id.as_str()) {
        return finish(
            ctx,
            status_for(ctx.layout, &request.command_id, &request.release_id, "ok", ""),
        );
    }
    if let Some(outstanding) = outstanding
        && (request.release_id == outstanding.baseline
            || request.release_id == record.release_id
            || current.as_deref() == Some(request.release_id.as_str()))
    {
        return refuse_without_clearing(
            ctx,
            refused(
                ctx.layout,
                request,
                "unresolved_rollback: a prior attempt is still unresolved",
            ),
            Some(outstanding),
        );
    }

    let key = resolve_pinned_key(ctx.layout)?;
    let (manifest, signature, package) = match fetch_verified(ctx.fetcher, request, &record.package_origin, &key) {
        Ok(parts) => parts,
        Err(error) => {
            return refuse_without_clearing(ctx, refused(ctx.layout, request, &error.to_string()), outstanding);
        }
    };
    let platform = Platform::host()?;
    let verified = match manifest::verify_release(&key, &manifest, &signature, &package, platform) {
        Ok(verified) => verified,
        Err(error) => {
            return refuse_without_clearing(ctx, refused(ctx.layout, request, &error.to_string()), outstanding);
        }
    };
    if verified.manifest.release_id != request.release_id {
        return refuse_without_clearing(
            ctx,
            refused(
                ctx.layout,
                request,
                "manifest_malformed: release_id does not match the offer",
            ),
            outstanding,
        );
    }
    if let Err(error) = manifest::require_origin(&verified.manifest.package.url, &record.package_origin) {
        return refuse_without_clearing(ctx, refused(ctx.layout, request, &error.to_string()), outstanding);
    }

    let candidate = Candidate::builder()
        .request(request)
        .account(account)
        .oldest_journal_version(verified.manifest.oldest_journal_version)
        .package_sha256(&verified.manifest.package.sha256)
        .package(&package)
        // An unresolved rollback keeps its original baseline: `current` may be
        // the failed candidate, and `previous` must stay the last good release.
        .from(
            outstanding
                .map(|outstanding| outstanding.baseline.clone())
                .or_else(|| current.clone())
                .unwrap_or_else(|| record.release_id.clone()),
        )
        .to(request.release_id.clone())
        .nonce(fresh_nonce()?)
        .enrolled(record_enrolled(ctx.layout, &record))
        .running(
            outstanding
                .map(|outstanding| outstanding.running.clone())
                .filter(|running| !running.is_empty())
                .or_else(|| current.clone())
                .unwrap_or_else(|| record.release_id.clone()),
        )
        .baseline_journal(outstanding.map(|outstanding| outstanding.journal_version).unwrap_or(0))
        .held_phase(held_phase(outstanding))
        .build();

    if candidate.enrolled {
        // Durable before the daemon is stopped, so a crash from here on still
        // knows the daemon has to be started again. The baseline journal version
        // is carried forward unchanged.
        write_marker(
            ctx.layout,
            &marker(
                &candidate.from,
                &candidate.to,
                "preparing",
                &request.command_id,
                candidate.baseline_journal,
                &candidate.nonce,
                &candidate.running,
            )
            .with_held(&candidate.held_phase),
        )?;
        write_active(ctx.layout)?;
        crash(ctx, "after_prepare_marker")?;
    }
    let journal_version = match switch(ctx, &candidate) {
        Ok(Gate::Switched { journal_version }) => journal_version,
        Ok(Gate::Refused(status)) if candidate.enrolled => {
            return restore(ctx, &candidate, candidate.baseline_journal, status);
        }
        Ok(Gate::Refused(status)) => return finish(ctx, status),
        Err(error) if injected_crash(&error) => return Err(error),
        Err(error) if candidate.enrolled => {
            let status = status_for(
                ctx.layout,
                &request.command_id,
                &request.release_id,
                "failed",
                &format!("{error:#}"),
            );
            return restore(ctx, &candidate, candidate.baseline_journal, status);
        }
        Err(error) => return Err(error),
    };
    crash(ctx, "after_switch")?;
    let (from, to, command_id, nonce) = (&candidate.from, &candidate.to, &request.command_id, &candidate.nonce);
    if candidate.enrolled {
        let ids = SwitchIds {
            from,
            to,
            command_id,
            journal_version,
            running: &candidate.running,
        };
        if let Err(error) = prepare_attempt(ctx, to, nonce) {
            return rollback(ctx, ids, nonce, &format!("attempt file: {error:#}"));
        }
        if let Err(error) = ctx.host.restart_service() {
            return rollback(ctx, ids, nonce, &format!("restart failed: {error:#}"));
        }
        crash(ctx, "after_restart")?;
        if let Err(error) = wait_switched(ctx, to, nonce, Cancellation::Forward) {
            return rollback(ctx, ids, nonce, &error.to_string());
        }
    }
    commit(
        ctx,
        &record,
        SwitchIds {
            from,
            to,
            command_id,
            journal_version,
            running: &candidate.running,
        },
        nonce,
    )
}

/// Stops the daemon, gates on the journal, unpacks the release and moves
/// `current`. Any failure leaves `current` where it was or pointing at `to`;
/// the caller puts the daemon back.
fn switch(ctx: &Apply<'_>, candidate: &Candidate<'_>) -> Result<Gate> {
    if candidate.enrolled {
        stop_and_quiesce(ctx, Cancellation::Forward)?;
        crash(ctx, "after_stop")?;
    }
    check_interrupt()?;
    let observed = match read_journal_version(ctx, &candidate.account)? {
        None => 0,
        Some(version) => {
            if candidate.oldest_journal_version > version {
                return Ok(Gate::Refused(refused(
                    ctx.layout,
                    candidate.request,
                    &ManifestError::JournalIncompatible {
                        oldest: candidate.oldest_journal_version,
                        current: version,
                    }
                    .to_string(),
                )));
            }
            version
        }
    };
    // Follow-on attempts keep the original baseline's readable version. A later
    // candidate that can read a migrated journal must not re-anchor rollback.
    let journal_version = if candidate.baseline_journal != 0 {
        candidate.baseline_journal
    } else {
        observed
    };
    install_release(ctx.layout, &candidate.to, candidate.package, candidate.package_sha256)?;
    write_marker(
        ctx.layout,
        &marker(
            &candidate.from,
            &candidate.to,
            "staged",
            &candidate.request.command_id,
            journal_version,
            &candidate.nonce,
            &candidate.running,
        )
        .with_held(&candidate.held_phase),
    )?;
    crash(ctx, "after_stage")?;
    point_previous(ctx.layout, &candidate.from)?;
    write_marker(
        ctx.layout,
        &marker(
            &candidate.from,
            &candidate.to,
            "switched",
            &candidate.request.command_id,
            journal_version,
            &candidate.nonce,
            &candidate.running,
        )
        .with_held(&candidate.held_phase),
    )?;
    point_current(ctx.layout, &candidate.to)?;
    Ok(Gate::Switched { journal_version })
}

/// Stops the daemon and waits until the service manager reports no running
/// instance, so the journal has no writer and nothing can relaunch one.
fn stop_and_quiesce(ctx: &Apply<'_>, cancellation: Cancellation) -> Result<()> {
    cancellation.check()?;
    ctx.host.stop_service().context("stop failed")?;
    let started = Instant::now();
    loop {
        cancellation.check()?;
        if ctx.host.service_instance()?.is_none() {
            return Ok(());
        }
        if started.elapsed() >= ctx.health.deadline {
            bail!(
                "stop_failed: the spoke service still had a running instance {:?} after it was stopped",
                ctx.health.deadline
            );
        }
        std::thread::sleep(ctx.health.poll);
    }
}

/// Puts the unchanged current release back in service after a refusal or an
/// error that came after the daemon was stopped, and proves it healthy with
/// the same contract a forward upgrade must meet. The restart outcome is part
/// of the terminal status.
fn restore(ctx: &Apply<'_>, candidate: &Candidate<'_>, journal_version: u32, outcome: Status) -> Result<Status> {
    restore_service(
        ctx,
        SwitchIds {
            from: &candidate.from,
            to: &candidate.to,
            command_id: &candidate.request.command_id,
            journal_version,
            running: &candidate.running,
        },
        outcome,
    )
}

fn restore_service(ctx: &Apply<'_>, ids: SwitchIds<'_>, outcome: Status) -> Result<Status> {
    let SwitchIds {
        from,
        to,
        command_id,
        journal_version,
        running: restart,
    } = ids;
    let held = read_marker(ctx.layout)
        .map(|marker| marker.held_phase)
        .filter(|phase| !phase.is_empty())
        .unwrap_or_default();
    let nonce = match fresh_nonce() {
        Ok(nonce) => nonce,
        Err(error) => {
            return unresolved_keep(
                ctx,
                ids,
                "restore_failed",
                &format!("{}; cannot mint restart nonce: {error:#}", outcome.error),
            );
        }
    };
    if let Err(error) = write_marker(
        ctx.layout,
        &marker(from, to, "restoring", command_id, journal_version, &nonce, restart).with_held(&held),
    ) {
        return unresolved_keep(
            ctx,
            ids,
            "restore_failed",
            &format!("{}; cannot write restore marker: {error:#}", outcome.error),
        );
    }
    let _ = write_active(ctx.layout);
    crash(ctx, "after_restore_marker")?;
    // Restart the release that was actually running. Never point `current` at a
    // baseline that cannot read the journal that release left behind.
    if current_release(ctx.layout).as_deref() != Some(restart)
        && let Err(error) = point_current(ctx.layout, restart)
    {
        return unresolved_keep(
            ctx,
            ids,
            "restore_failed",
            &format!("{}; cannot point current at {restart}: {error:#}", outcome.error),
        );
    }
    let _ = prepare_attempt(ctx, restart, &nonce);
    let healthy = ctx
        .host
        .restart_service()
        .map_err(|error| anyhow::anyhow!("restart failed: {error:#}"))
        .and_then(|()| wait_switched(ctx, restart, &nonce, Cancellation::Revert(Instant::now())));
    let status = match healthy {
        Ok(()) => {
            let mut status = outcome;
            status.running_release_id = current_release(ctx.layout).unwrap_or_default();
            status.error = if status.error.is_empty() {
                format!("service restarted on {restart}")
            } else {
                format!("{}; service restarted on {restart}", status.error)
            };
            status
        }
        Err(error) => status_for(
            ctx.layout,
            command_id,
            to,
            "restore_failed",
            &format!("{}; restarting {restart} not confirmed: {error:#}", outcome.error),
        ),
    };
    let mut status = status;
    if status.result != "restore_failed" {
        overlay_held(&mut status, &held);
    }
    let _ = write_status_file(ctx.layout, &status);
    let _ = remove_request(ctx.layout);
    if status.result == "restore_failed" {
        let _ = write_marker(
            ctx.layout,
            &marker(from, to, "restore_failed", command_id, journal_version, &nonce, restart).with_held(&held),
        );
        let _ = clear_active(ctx.layout);
    } else if !held.is_empty() {
        let _ = write_marker(
            ctx.layout,
            &marker(from, to, &held, command_id, journal_version, &nonce, restart).with_held(&held),
        );
        let _ = clear_active(ctx.layout);
    } else {
        let _ = clear_in_progress(ctx.layout);
        let _ = clear_active(ctx.layout);
    }
    Ok(status)
}

fn commit(ctx: &Apply<'_>, record: &InstallRecord, ids: SwitchIds<'_>, nonce: &str) -> Result<Status> {
    let SwitchIds {
        from,
        to,
        command_id,
        journal_version,
        running,
    } = ids;
    write_marker(
        ctx.layout,
        &marker(from, to, "committing", command_id, journal_version, nonce, running),
    )?;
    persist_record(ctx.layout, record, to)?;
    crash(ctx, "after_persist_record")?;
    let status = status_for(ctx.layout, command_id, to, "ok", "");
    write_status_file(ctx.layout, &status)?;
    crash(ctx, "after_status")?;
    write_marker(
        ctx.layout,
        &marker(from, to, "committed", command_id, journal_version, nonce, running),
    )?;
    finish(ctx, status)
}

fn wait_switched(ctx: &Apply<'_>, release_id: &str, nonce: &str, cancellation: Cancellation) -> Result<()> {
    let started = Instant::now();
    let mut seen: Option<(u32, u64, Instant)> = None;
    loop {
        cancellation.check()?;
        let instance = ctx.host.service_instance()?;
        if let Some(current) = instance {
            let ready = ctx.probe.ready(release_id, current.0, nonce)?;
            let stable = match seen {
                Some((pid, runs, at)) if (pid, runs) == current => at.elapsed() >= ctx.health.stable,
                _ => {
                    seen = Some((current.0, current.1, Instant::now()));
                    ctx.health.stable.is_zero()
                }
            };
            if stable && ready {
                return Ok(());
            }
        } else {
            seen = None;
        }
        if started.elapsed() >= ctx.health.deadline {
            bail!(
                "health_failed: lock, journal, inventory or management handshake did not succeed within {:?}",
                ctx.health.deadline
            );
        }
        std::thread::sleep(ctx.health.poll);
    }
}

/// Moves `current` back to `from` and requires the full health contract of the
/// restored release; bare liveness is not accepted. An outcome that cannot be
/// resolved here keeps its marker, so the original baseline survives, and
/// retires the request so a new offer is not refused as busy.
fn rollback(ctx: &Apply<'_>, ids: SwitchIds<'_>, forward_nonce: &str, error: &str) -> Result<Status> {
    let SwitchIds {
        from,
        to,
        command_id,
        journal_version,
        running,
    } = ids;
    let nonce = match fresh_nonce() {
        Ok(nonce) => nonce,
        Err(fail) => {
            return unresolved_keep(
                ctx,
                ids,
                "rollback_failed",
                &format!("{error}; cannot mint rollback nonce: {fail:#}"),
            );
        }
    };
    if let Err(fail) = write_marker(
        ctx.layout,
        &marker(
            from,
            to,
            "rollback_quiescing",
            command_id,
            journal_version,
            &nonce,
            running,
        ),
    ) {
        return unresolved_keep(
            ctx,
            ids,
            "rollback_failed",
            &format!("{error}; cannot write rollback marker: {fail:#}"),
        );
    }
    let _ = write_active(ctx.layout);
    if let Err(fail) = stop_and_quiesce(ctx, Cancellation::Revert(Instant::now())) {
        let _ = restart_best(ctx, to, &nonce);
        return unresolved_keep(
            ctx,
            ids,
            "rollback_failed",
            &format!("{error}; stop failed before journal read: {fail:#}"),
        );
    }
    let account = match service_account_from_record(ctx) {
        Ok(account) => account,
        Err(fail) => {
            let _ = restart_best(ctx, to, &nonce);
            return unresolved_keep(ctx, ids, "rollback_failed", &format!("{error}; {fail:#}"));
        }
    };
    let current_journal = match read_journal_version(ctx, &account) {
        Ok(version) => version,
        Err(fail) => {
            let _ = restart_best(ctx, to, &nonce);
            return unresolved_keep(
                ctx,
                ids,
                "rollback_failed",
                &format!("{error}; cannot read journal version: {fail:#}"),
            );
        }
    };
    if let Some(current) = current_journal
        && current > journal_version
        && journal_version != 0
    {
        let _ = write_marker(
            ctx.layout,
            &marker(
                from,
                to,
                "rollback_refused",
                command_id,
                journal_version,
                forward_nonce,
                to,
            ),
        );
        let status = status_for(
            ctx.layout,
            command_id,
            to,
            "rollback_refused_journal_migrated",
            &format!("journal migrated to v{current}; previous reads through v{journal_version}"),
        );
        let _ = write_status_file(ctx.layout, &status);
        let _ = remove_request(ctx.layout);
        let _ = clear_active(ctx.layout);
        let _ = restart_best(ctx, to, &nonce);
        return Ok(status);
    }
    if let Err(fail) = write_marker(
        ctx.layout,
        &marker(from, to, "rolling_back", command_id, journal_version, &nonce, running),
    ) {
        let _ = restart_best(ctx, from, &nonce);
        return unresolved_keep(ctx, ids, "rollback_failed", &format!("{error}; {fail:#}"));
    }
    crash(ctx, "after_rollback_marker")?;
    if let Err(fail) = point_current(ctx.layout, from) {
        let _ = restart_best(ctx, to, &nonce);
        return unresolved_keep(
            ctx,
            ids,
            "rollback_failed",
            &format!("{error}; cannot point current at {from}: {fail:#}"),
        );
    }
    crash(ctx, "after_rollback_switch")?;
    let _ = prepare_attempt(ctx, from, &nonce);
    let restored = ctx
        .host
        .restart_service()
        .map_err(|error| anyhow::anyhow!("restart failed: {error:#}"))
        .and_then(|()| wait_switched(ctx, from, &nonce, Cancellation::Revert(Instant::now())));
    if let Err(failure) = restored {
        let _ = write_marker(
            ctx.layout,
            &marker(
                from,
                to,
                "rollback_failed",
                command_id,
                journal_version,
                &nonce,
                running,
            ),
        );
        let status = status_for(
            ctx.layout,
            command_id,
            to,
            "rollback_failed",
            &format!("{error}; restoring {from} not confirmed: {failure:#}"),
        );
        let _ = write_status_file(ctx.layout, &status);
        let _ = remove_request(ctx.layout);
        let _ = clear_active(ctx.layout);
        return Ok(status);
    }
    crash(ctx, "after_rollback_health")?;
    let status = status_for(
        ctx.layout,
        command_id,
        to,
        "rolled_back",
        &format!("{error}; restored {from}"),
    );
    let _ = write_status_file(ctx.layout, &status);
    let _ = remove_request(ctx.layout);
    let _ = clear_in_progress(ctx.layout);
    let _ = clear_active(ctx.layout);
    Ok(status)
}

fn restart_best(ctx: &Apply<'_>, release_id: &str, nonce: &str) -> Result<()> {
    let _ = prepare_attempt(ctx, release_id, nonce);
    ctx.host.restart_service()?;
    let _ = wait_switched(ctx, release_id, nonce, Cancellation::Revert(Instant::now()));
    Ok(())
}

fn unresolved_keep(ctx: &Apply<'_>, ids: SwitchIds<'_>, result: &str, error: &str) -> Result<Status> {
    let SwitchIds {
        from,
        to,
        command_id,
        journal_version,
        running,
    } = ids;
    let status = status_for(ctx.layout, command_id, to, result, error);
    let _ = write_status_file(ctx.layout, &status);
    let _ = write_marker(
        ctx.layout,
        &marker(from, to, result, command_id, journal_version, "", running),
    );
    let _ = remove_request(ctx.layout);
    let _ = clear_active(ctx.layout);
    Ok(status)
}

fn unresolved(status: &Status) -> bool {
    matches!(
        status.result.as_str(),
        "rollback_failed" | "rollback_refused_journal_migrated" | "restore_failed"
    )
}

/// Finishes whatever an earlier run left behind. Returns the baseline a new
/// request must respect when the previous attempt could not be resolved.
fn recover(ctx: &Apply<'_>) -> Result<Option<Outstanding>> {
    let path = ctx.layout.on_disk(&ctx.layout.in_progress_path());
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let marker: InProgress = serde_json::from_slice(&bytes).context("upgrade-in-progress is not valid JSON")?;
    let enrolled = InstallRecord::read(&ctx.layout.on_disk(&ctx.layout.config_path()))
        .map(|record| record_enrolled(ctx.layout, &record))
        .unwrap_or(false);
    let rollback_again = |reason: &str| rollback(ctx, SwitchIds::of(&marker), &marker.nonce, reason);
    let restore_again = |reason: &str| {
        let outcome = status_for(ctx.layout, &marker.command_id, &marker.to, "failed", reason);
        restore_service(ctx, SwitchIds::of(&marker), outcome)
    };
    let status = match marker.phase.as_str() {
        "preparing" if enrolled => Some(restore_again("interrupted:prepare")?),
        "staged" if current_release(ctx.layout).as_deref() == Some(marker.to.as_str()) => {
            Some(rollback_again("interrupted:switch")?)
        }
        "staged" if enrolled => Some(restore_again("interrupted:stage")?),
        "switched" | "rollback_quiescing" => Some(rollback_again("interrupted:health")?),
        "committing" => {
            if current_release(ctx.layout).as_deref() == Some(marker.to.as_str()) {
                if let Ok(record) = InstallRecord::read(&ctx.layout.on_disk(&ctx.layout.config_path())) {
                    persist_record(ctx.layout, &record, &marker.to)?;
                }
                let status = status_for(ctx.layout, &marker.command_id, &marker.to, "ok", "");
                write_status_file(ctx.layout, &status)?;
                let _ = remove_request(ctx.layout);
                clear_in_progress(ctx.layout)?;
                let _ = clear_active(ctx.layout);
                None
            } else {
                Some(rollback_again("interrupted:commit")?)
            }
        }
        "committed" => {
            let status = status_for(ctx.layout, &marker.command_id, &marker.to, "ok", "");
            write_status_file(ctx.layout, &status)?;
            let _ = remove_request(ctx.layout);
            clear_in_progress(ctx.layout)?;
            let _ = clear_active(ctx.layout);
            None
        }
        "rolling_back" | "rollback_failed" => Some(rollback_again("interrupted:rollback")?),
        "restoring" | "restore_failed" => Some(restore_again("interrupted:restore")?),
        "rollback_refused" => {
            let status = status_for(
                ctx.layout,
                &marker.command_id,
                &marker.to,
                "rollback_refused_journal_migrated",
                "previous cannot read the migrated journal",
            );
            write_status_file(ctx.layout, &status)?;
            Some(status)
        }
        _ => {
            clear_in_progress(ctx.layout)?;
            let _ = clear_active(ctx.layout);
            None
        }
    };
    // Restore of a follow-on attempt may put the unresolved marker back while
    // the action status is `failed`. Only a marker that is still on disk is
    // outstanding: a successful rollback or commit has already cleared it.
    Ok(read_marker(ctx.layout).and_then(|marker| outstanding_from(&marker, status)))
}

fn outstanding_from(marker: &InProgress, status: Option<Status>) -> Option<Outstanding> {
    let status = match status {
        Some(status) if unresolved(&status) => status,
        _ => match marker.phase.as_str() {
            "rollback_refused" => status_for_unresolved(
                marker,
                "rollback_refused_journal_migrated",
                "previous cannot read the migrated journal",
            ),
            "rollback_failed" => status_for_unresolved(marker, "rollback_failed", "rollback is unresolved"),
            "restore_failed" => status_for_unresolved(marker, "restore_failed", "restore is unresolved"),
            _ => return None,
        },
    };
    Some(Outstanding {
        baseline: marker.from.clone(),
        journal_version: marker.journal_version,
        running: restart_of(marker).to_owned(),
        status,
    })
}

fn status_for_unresolved(marker: &InProgress, result: &str, error: &str) -> Status {
    Status {
        schema: STATUS_SCHEMA,
        command_id: marker.command_id.clone(),
        release_id: marker.to.clone(),
        result: result.to_owned(),
        error: error.to_owned(),
        running_release_id: marker.running.clone(),
        unresolved_result: result.to_owned(),
        unresolved_error: error.to_owned(),
        unresolved_release_id: marker.to.clone(),
    }
}

fn restart_of(marker: &InProgress) -> &str {
    if marker.running.is_empty() {
        marker.from.as_str()
    } else {
        marker.running.as_str()
    }
}

impl<'a> SwitchIds<'a> {
    fn of(marker: &'a InProgress) -> Self {
        Self {
            from: &marker.from,
            to: &marker.to,
            command_id: &marker.command_id,
            journal_version: marker.journal_version,
            running: restart_of(marker),
        }
    }
}

fn finish(ctx: &Apply<'_>, status: Status) -> Result<Status> {
    write_status_file(ctx.layout, &status)?;
    crash(ctx, "after_status")?;
    let _ = remove_request(ctx.layout);
    crash(ctx, "after_remove_request")?;
    clear_in_progress(ctx.layout)?;
    let _ = clear_active(ctx.layout);
    crash(ctx, "after_clear_in_progress")?;
    Ok(status)
}

/// Writes a refused offer's status and drops the claimed request without
/// clearing an outstanding rollback/restore marker.
fn refuse_without_clearing(ctx: &Apply<'_>, mut status: Status, outstanding: Option<&Outstanding>) -> Result<Status> {
    if let Some(outstanding) = outstanding {
        status.unresolved_result = outstanding.status.result.clone();
        status.unresolved_error = outstanding.status.error.clone();
        status.unresolved_release_id = outstanding.status.release_id.clone();
    }
    write_status_file(ctx.layout, &status)?;
    crash(ctx, "after_status")?;
    let _ = remove_request(ctx.layout);
    crash(ctx, "after_remove_request")?;
    if outstanding.is_none() {
        clear_in_progress(ctx.layout)?;
        crash(ctx, "after_clear_in_progress")?;
    }
    // Always drop `upgrade-active` on a request that never owned the marker so
    // PathExists/KeepAlive do not loop on a refused offer.
    let _ = clear_active(ctx.layout);
    Ok(status)
}

fn read_marker(layout: &Layout) -> Option<InProgress> {
    let bytes = std::fs::read(layout.on_disk(&layout.in_progress_path())).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn fresh_nonce() -> Result<String> {
    Ok(format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ))
}

fn fetch_verified(
    fetcher: &dyn Fetcher,
    request: &Request,
    origin: &str,
    key: &TrustedKey,
) -> Result<(Vec<u8>, String, Vec<u8>), ManifestError> {
    manifest::require_origin(&request.manifest_url, origin)?;
    let signature_url = format!("{}.sig", request.manifest_url);
    manifest::require_origin(&signature_url, origin)?;
    let manifest = fetcher
        .get(&request.manifest_url)
        .map_err(|error| ManifestError::Malformed(error.to_string()))?;
    if manifest::sha256_hex(&manifest) != request.manifest_sha256 {
        return Err(ManifestError::DigestMismatch(
            "the fetched manifest digest differs from the offer".into(),
        ));
    }
    let signature = String::from_utf8(
        fetcher
            .get(&signature_url)
            .map_err(|error| ManifestError::Malformed(error.to_string()))?,
    )
    .map_err(|_| ManifestError::ManifestSignature)?;
    manifest::verify_signature(key, &manifest, &signature)?;
    let parsed: crate::manifest::Manifest =
        serde_json::from_slice(&manifest).map_err(|error| ManifestError::Malformed(error.to_string()))?;
    parsed.validate_for_fetch().map_err(ManifestError::Malformed)?;
    manifest::require_origin(&parsed.package.url, origin)?;
    let package = fetcher
        .get(&parsed.package.url)
        .map_err(|error| ManifestError::Malformed(error.to_string()))?;
    Ok((manifest, signature, package))
}

trait ManifestFetch {
    fn validate_for_fetch(&self) -> Result<(), String>;
}

impl ManifestFetch for crate::manifest::Manifest {
    fn validate_for_fetch(&self) -> Result<(), String> {
        if self.schema != manifest::SCHEMA {
            return Err(format!("unsupported schema {}", self.schema));
        }
        Ok(())
    }
}

fn load_request(ctx: &Apply<'_>) -> Result<Option<Request>> {
    let Some(bytes) = claim_request(ctx.layout)? else {
        return Ok(None);
    };
    // Durable before fetch/verify so a kill during those still retriggers the
    // path unit / KeepAlive (claimed + active both exist).
    write_active(ctx.layout)?;
    Ok(Some(
        serde_json::from_slice(&bytes).context("update request is not valid JSON")?,
    ))
}

/// Retires the claimed offer. A replacement sitting at `request.json` is never
/// touched: it arrived after the claim and belongs to the next helper run.
fn remove_request(layout: &Layout) -> Result<()> {
    clear_claimed(layout)
}

fn write_status_file(layout: &Layout, status: &Status) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(status)?;
    bytes.push(b'\n');
    write_status(layout, &bytes)?;
    Ok(())
}

fn marker(
    from: &str,
    to: &str,
    phase: &str,
    command_id: &str,
    journal_version: u32,
    nonce: &str,
    running: &str,
) -> InProgress {
    InProgress::builder()
        .from(from)
        .to(to)
        .phase(phase)
        .command_id(command_id)
        .journal_version(journal_version)
        .nonce(nonce)
        .running(running)
        .build()
}

fn write_marker(layout: &Layout, marker: &InProgress) -> Result<()> {
    let mut bytes = serde_json::to_vec(marker)?;
    bytes.push(b'\n');
    write_in_progress(layout, &bytes)?;
    Ok(())
}

fn status_for(layout: &Layout, command_id: &str, release_id: &str, result: &str, error: &str) -> Status {
    let unresolved = unresolved_result(result);
    Status {
        schema: STATUS_SCHEMA,
        command_id: command_id.to_owned(),
        release_id: release_id.to_owned(),
        result: result.to_owned(),
        error: error.to_owned(),
        running_release_id: current_release(layout).unwrap_or_default(),
        unresolved_result: if unresolved { result.to_owned() } else { String::new() },
        unresolved_error: if unresolved { error.to_owned() } else { String::new() },
        unresolved_release_id: if unresolved {
            release_id.to_owned()
        } else {
            String::new()
        },
    }
}

fn unresolved_result(result: &str) -> bool {
    matches!(
        result,
        "rollback_failed" | "rollback_refused_journal_migrated" | "restore_failed"
    )
}

fn held_phase(outstanding: Option<&Outstanding>) -> String {
    outstanding
        .map(|outstanding| match outstanding.status.result.as_str() {
            "rollback_refused_journal_migrated" => "rollback_refused".to_owned(),
            other => other.to_owned(),
        })
        .filter(|phase| !phase.is_empty())
        .unwrap_or_default()
}

fn overlay_held(status: &mut Status, held: &str) {
    if held.is_empty() {
        return;
    }
    status.unresolved_result = if held == "rollback_refused" {
        "rollback_refused_journal_migrated".to_owned()
    } else {
        held.to_owned()
    };
}

fn refused(layout: &Layout, request: &Request, error: &str) -> Status {
    status_for(layout, &request.command_id, &request.release_id, "refused", error)
}

fn record_enrolled(layout: &Layout, record: &InstallRecord) -> bool {
    let credentials = layout.on_disk(&layout.credentials_dir());
    ["cert.pem", "key.pem"]
        .iter()
        .all(|name| credentials.join(name).is_file())
        && !record.host_id.is_empty()
}

fn persist_record(layout: &Layout, record: &InstallRecord, release_id: &str) -> Result<()> {
    let updated = InstallRecord::builder()
        .schema(record.schema)
        .release_id(release_id)
        .platform(record.platform.clone())
        .user(record.user.clone())
        .host_id(record.host_id.clone())
        .hub_url(record.hub_url.clone())
        .package_origin(record.package_origin.clone())
        .build();
    let mut bytes = serde_json::to_vec_pretty(&updated)?;
    bytes.push(b'\n');
    install::write_logical(layout, &layout.config_path(), &bytes, 0o644)?;
    install::refresh_update_helper(layout)?;
    Ok(())
}

fn prepare_attempt(ctx: &Apply<'_>, release_id: &str, nonce: &str) -> Result<()> {
    injected(ctx, "attempt_write")?;
    let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "release_id": release_id,
        "nonce": nonce,
    }))?;
    bytes.push(b'\n');
    write_logical(ctx.layout, &ctx.layout.attempt_path(), &bytes, 0o644)?;
    let _ = unlink_service_file(ctx.layout, &ctx.layout.health_path());
    Ok(())
}

fn crash(ctx: &Apply<'_>, point: &str) -> Result<()> {
    if ctx.crash.as_deref() == Some(point) {
        if ctx.unwind_crash {
            bail!("injected crash at {point}");
        }
        // Abort the process so the helper cannot run its error-cleanup path.
        // Tests spawn the staged helper as a subprocess and restart it without
        // the hook.
        std::process::abort();
    }
    Ok(())
}

fn injected(ctx: &Apply<'_>, name: &str) -> Result<()> {
    if ctx.fail.as_deref() == Some(name) {
        bail!("injected failure: {name}");
    }
    Ok(())
}

fn read_journal_version(ctx: &Apply<'_>, account: &Account) -> Result<Option<u32>> {
    injected(ctx, "journal_read")?;
    journal_user_version(ctx.host, ctx.layout, account, ctx.inspector)
}

/// Reads SQLite `user_version` (WAL-aware) as the service user: the database,
/// its `-wal` and `-shm` files and the directory holding them are all the
/// service user's, so root never opens any of them. `inspector` is the
/// installer binary, run with its `journal-version` command. A missing file is
/// [`None`]; a short or malformed file is an error.
pub fn journal_user_version(
    host: &dyn Host,
    layout: &Layout,
    account: &Account,
    inspector: &Path,
) -> Result<Option<u32>> {
    let Some(path) = journal_sqlite_path(layout, account)? else {
        return Ok(None);
    };
    let output = host.run_as(
        account,
        &[],
        inspector,
        &["journal-version".into(), "--path".into(), path.display().to_string()],
    )?;
    ensure!(
        output.success,
        "cannot read the journal version as the service user: {}",
        output.output.trim()
    );
    output
        .output
        .lines()
        .find_map(|line| line.strip_prefix("user_version=")?.trim().parse().ok())
        .map(Some)
        .with_context(|| format!("unexpected journal inspector output: {}", output.output.trim()))
}

/// The unprivileged half of [`journal_user_version`]. It runs as the service
/// user, so it can open nothing that user could not already read; that, not a
/// symlink check, is what keeps root out of the service user's files.
pub fn read_user_version(path: &Path) -> Result<u32> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open {}", path.display()))?;
    conn.pragma_update(None, "query_only", true)
        .with_context(|| format!("cannot open {} read-only", path.display()))?;
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .with_context(|| format!("cannot read user_version from {}", path.display()))
}

pub fn write_request(path: &Path, request: &Request) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(request)?;
    bytes.push(b'\n');
    std::fs::write(path, bytes)?;
    Ok(())
}

fn service_account(ctx: &Apply<'_>, record: &InstallRecord) -> Result<Account> {
    ctx.host
        .lookup_user(&record.user)?
        .with_context(|| format!("service user {} is missing", record.user))
}

fn service_account_from_record(ctx: &Apply<'_>) -> Result<Account> {
    let record = InstallRecord::read(&ctx.layout.on_disk(&ctx.layout.config_path()))?;
    service_account(ctx, &record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revert_waits_ignore_termination_and_an_exhausted_forward_budget() {
        begin_helper_run();
        STARTED.with(|started| started.set(Some(Instant::now() - HELPER_BUDGET)));
        assert!(Cancellation::Forward.check().is_err());
        assert!(Cancellation::Revert(Instant::now()).check().is_ok());
        request_terminate();
        assert!(Cancellation::Forward.check().is_err());
        assert!(Cancellation::Revert(Instant::now()).check().is_ok());
        assert!(
            Cancellation::Revert(Instant::now() - Duration::from_secs(45))
                .check()
                .is_err()
        );
        begin_helper_run();
    }

    #[test]
    fn an_oversized_declared_length_is_refused_before_any_body_is_read() {
        assert!(check_declared_length(None).is_ok());
        assert!(check_declared_length(Some(FETCH_MAX_BYTES as u64)).is_ok());
        assert!(check_declared_length(Some(FETCH_MAX_BYTES as u64 + 1)).is_err());
    }

    #[test]
    fn a_chunked_body_is_cut_off_as_soon_as_it_passes_the_cap() {
        let mut body = Vec::new();
        let chunk = vec![0u8; 1024 * 1024];
        let mut accepted = 0usize;
        // A server that never declares a length and never stops.
        let failure = loop {
            match accept_chunk(&mut body, &chunk) {
                Ok(()) => accepted += 1,
                Err(error) => break error,
            }
        };
        assert_eq!(accepted, FETCH_MAX_BYTES / chunk.len());
        assert!(body.len() <= FETCH_MAX_BYTES, "buffered {} bytes", body.len());
        assert!(failure.to_string().contains("exceeded"));
    }
}
