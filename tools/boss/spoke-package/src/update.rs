//! Privileged update helper: the service user names a release; root fetches,
//! verifies, switches and rolls back.
//!
//! Nothing the service user writes is trusted beyond "which HTTPS release to
//! try". The helper re-fetches the manifest, signature and package from the
//! origin pinned at provisioning, and verifies them against the root-pinned
//! release key before it touches `current`.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::install::{
    self, Account, Health, Host, InstallRecord, Layout, clear_in_progress, current_release, install_release,
    journal_sqlite_path, point_current, point_previous, read_service_file, resolve_pinned_key, unlink_service_file,
    wait_healthy, write_in_progress, write_logical, write_status,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct InProgress {
    from: String,
    to: String,
    phase: String,
    #[serde(default)]
    command_id: String,
    #[serde(default)]
    journal_version: u32,
    #[serde(default)]
    nonce: String,
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
/// origin pin on every redirect hop.
pub struct HttpFetcher {
    pub origin: String,
}

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("cannot start fetch runtime")?;
        runtime.block_on(fetch_https(&self.origin, url))
    }
}

async fn fetch_https(origin: &str, start: &str) -> Result<Vec<u8>> {
    manifest::require_origin(start, origin)?;
    let policy = boss_http_retry::RetryPolicy::new(4, Duration::from_millis(250), Duration::from_secs(30));
    let mut url = start.to_owned();
    for hop in 0..=REDIRECT_HOPS {
        let mut last_error = None;
        for attempt in 1..=policy.max_attempts {
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
                Ok(response) if response.status().is_success() => {
                    let bytes = response.bytes().await.context("body")?;
                    ensure!(
                        bytes.len() <= FETCH_MAX_BYTES,
                        "fetch exceeded {FETCH_MAX_BYTES} bytes"
                    );
                    return Ok(bytes.to_vec());
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

/// True once the new daemon has locked, opened the journal, inventoried and
/// completed a management handshake for this attempt.
pub struct FileProbe<'a> {
    pub layout: &'a Layout,
    pub account: Account,
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
            && flag("handshake_ok")
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
    pub crash: Option<&'static str>,
}

pub fn apply(ctx: &Apply<'_>) -> Result<Status> {
    recover(ctx)?;
    let request = match load_request(ctx) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(status_for(ctx.layout, "", "", "idle", "")),
        Err(error) => {
            let status = status_for(ctx.layout, "", "", "refused", &format!("request_malformed: {error:#}"));
            let _ = remove_request(ctx.layout, "");
            write_status_file(ctx.layout, &status)?;
            return Ok(status);
        }
    };
    match apply_request(ctx, &request) {
        Ok(status) => Ok(status),
        Err(error) => {
            let status = status_for(
                ctx.layout,
                &request.command_id,
                &request.release_id,
                "failed",
                &format!("{error:#}"),
            );
            let _ = write_status_file(ctx.layout, &status);
            Err(error)
        }
    }
}

fn apply_request(ctx: &Apply<'_>, request: &Request) -> Result<Status> {
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
        return finish(
            ctx,
            refused(ctx.layout, request, "manifest_malformed: request is not a usable offer"),
        );
    }
    if let Err(error) = manifest::require_origin(&request.manifest_url, &record.package_origin) {
        return finish(ctx, refused(ctx.layout, request, &error.to_string()));
    }
    if request.release_id == record.release_id {
        return finish(
            ctx,
            status_for(ctx.layout, &request.command_id, &request.release_id, "ok", ""),
        );
    }

    let key = resolve_pinned_key(ctx.layout)?;
    let (manifest, signature, package) = match fetch_verified(ctx.fetcher, request, &record.package_origin, &key) {
        Ok(parts) => parts,
        Err(error) => return finish(ctx, refused(ctx.layout, request, &error.to_string())),
    };
    let platform = Platform::host()?;
    let verified = match manifest::verify_release(&key, &manifest, &signature, &package, platform) {
        Ok(verified) => verified,
        Err(error) => return finish(ctx, refused(ctx.layout, request, &error.to_string())),
    };
    if verified.manifest.release_id != request.release_id {
        return finish(
            ctx,
            refused(
                ctx.layout,
                request,
                "manifest_malformed: release_id does not match the offer",
            ),
        );
    }
    if let Err(error) = manifest::require_origin(&verified.manifest.package.url, &record.package_origin) {
        return finish(ctx, refused(ctx.layout, request, &error.to_string()));
    }

    let enrolled = record_enrolled(ctx.layout, &record);
    if enrolled {
        ctx.host.stop_service().context("stop failed")?;
    }
    let journal_version = match journal_user_version(ctx.layout, &account)? {
        None => 0,
        Some(version) => {
            if verified.manifest.oldest_journal_version > version {
                return finish(
                    ctx,
                    refused(
                        ctx.layout,
                        request,
                        &ManifestError::JournalIncompatible {
                            oldest: verified.manifest.oldest_journal_version,
                            current: version,
                        }
                        .to_string(),
                    ),
                );
            }
            version
        }
    };

    let from = current_release(ctx.layout).unwrap_or_else(|| record.release_id.clone());
    let to = request.release_id.clone();
    let nonce = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
    install_release(ctx.layout, &to, &package, &verified.manifest.package.sha256)?;
    write_marker(
        ctx.layout,
        Marker {
            from: &from,
            to: &to,
            phase: "staged",
            command_id: &request.command_id,
            journal_version,
            nonce: &nonce,
        },
    )?;
    crash(ctx, "after_stage")?;
    point_previous(ctx.layout, &from)?;
    write_marker(
        ctx.layout,
        Marker {
            from: &from,
            to: &to,
            phase: "switched",
            command_id: &request.command_id,
            journal_version,
            nonce: &nonce,
        },
    )?;
    point_current(ctx.layout, &to)?;
    crash(ctx, "after_switch")?;
    if enrolled {
        prepare_attempt(ctx.layout, &to, &nonce)?;
        if let Err(error) = ctx.host.restart_service() {
            return rollback(
                ctx,
                &from,
                &to,
                &request.command_id,
                journal_version,
                &nonce,
                &format!("restart failed: {error:#}"),
            );
        }
        crash(ctx, "after_restart")?;
        if let Err(error) = wait_switched(ctx, &to, &nonce) {
            return rollback(
                ctx,
                &from,
                &to,
                &request.command_id,
                journal_version,
                &nonce,
                &error.to_string(),
            );
        }
    }
    commit(ctx, &record, &from, &to, &request.command_id, journal_version, &nonce)
}

fn commit(
    ctx: &Apply<'_>,
    record: &InstallRecord,
    from: &str,
    to: &str,
    command_id: &str,
    journal_version: u32,
    nonce: &str,
) -> Result<Status> {
    write_marker(
        ctx.layout,
        Marker {
            from,
            to,
            phase: "committing",
            command_id,
            journal_version,
            nonce,
        },
    )?;
    persist_record(ctx.layout, record, to)?;
    crash(ctx, "after_persist_record")?;
    let status = status_for(ctx.layout, command_id, to, "ok", "");
    write_status_file(ctx.layout, &status)?;
    crash(ctx, "after_status")?;
    write_marker(
        ctx.layout,
        Marker {
            from,
            to,
            phase: "committed",
            command_id,
            journal_version,
            nonce,
        },
    )?;
    finish(ctx, status)
}

fn wait_switched(ctx: &Apply<'_>, release_id: &str, nonce: &str) -> Result<()> {
    let started = Instant::now();
    let mut seen: Option<(u32, u64, Instant)> = None;
    loop {
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

fn rollback(
    ctx: &Apply<'_>,
    from: &str,
    to: &str,
    command_id: &str,
    journal_version: u32,
    nonce: &str,
    error: &str,
) -> Result<Status> {
    let account = service_account_from_record(ctx)?;
    if let Some(current) = journal_user_version(ctx.layout, &account)?
        && current > journal_version
        && journal_version != 0
    {
        write_marker(
            ctx.layout,
            Marker {
                from,
                to,
                phase: "rollback_refused",
                command_id,
                journal_version,
                nonce,
            },
        )?;
        let status = status_for(
            ctx.layout,
            command_id,
            to,
            "rollback_refused_journal_migrated",
            &format!("journal migrated to v{current}; previous reads through v{journal_version}"),
        );
        write_status_file(ctx.layout, &status)?;
        return Ok(status);
    }
    write_marker(
        ctx.layout,
        Marker {
            from,
            to,
            phase: "rolling_back",
            command_id,
            journal_version,
            nonce,
        },
    )?;
    crash(ctx, "after_rollback_marker")?;
    point_current(ctx.layout, from)?;
    crash(ctx, "after_rollback_switch")?;
    prepare_attempt(ctx.layout, from, nonce)?;
    if let Err(error) = ctx.host.restart_service() {
        let status = status_for(
            ctx.layout,
            command_id,
            to,
            "rollback_failed",
            &format!("{error:#}; restored {from} not confirmed"),
        );
        write_status_file(ctx.layout, &status)?;
        return Ok(status);
    }
    if let Err(error) = wait_switched(ctx, from, nonce).or_else(|_| wait_healthy(ctx.host, &ctx.health, ctx.layout)) {
        let status = status_for(
            ctx.layout,
            command_id,
            to,
            "rollback_failed",
            &format!("{error:#}; restored {from} not confirmed"),
        );
        write_status_file(ctx.layout, &status)?;
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
    write_status_file(ctx.layout, &status)?;
    let _ = remove_request(ctx.layout, command_id);
    clear_in_progress(ctx.layout)?;
    Ok(status)
}

fn recover(ctx: &Apply<'_>) -> Result<()> {
    let path = ctx.layout.on_disk(&ctx.layout.in_progress_path());
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let marker: InProgress = serde_json::from_slice(&bytes).context("upgrade-in-progress is not valid JSON")?;
    match marker.phase.as_str() {
        "staged" => {
            if current_release(ctx.layout).as_deref() == Some(marker.to.as_str()) {
                persist_recovery(
                    ctx,
                    rollback(
                        ctx,
                        &marker.from,
                        &marker.to,
                        &marker.command_id,
                        marker.journal_version,
                        &marker.nonce,
                        "interrupted:switch",
                    )?,
                )?;
            } else {
                clear_in_progress(ctx.layout)?;
            }
        }
        "switched" => {
            persist_recovery(
                ctx,
                rollback(
                    ctx,
                    &marker.from,
                    &marker.to,
                    &marker.command_id,
                    marker.journal_version,
                    &marker.nonce,
                    "interrupted:health",
                )?,
            )?;
        }
        "committing" => {
            if current_release(ctx.layout).as_deref() == Some(marker.to.as_str()) {
                if let Ok(record) = InstallRecord::read(&ctx.layout.on_disk(&ctx.layout.config_path())) {
                    persist_record(ctx.layout, &record, &marker.to)?;
                }
                let status = status_for(ctx.layout, &marker.command_id, &marker.to, "ok", "");
                write_status_file(ctx.layout, &status)?;
                let _ = remove_request(ctx.layout, &marker.command_id);
                clear_in_progress(ctx.layout)?;
            } else {
                persist_recovery(
                    ctx,
                    rollback(
                        ctx,
                        &marker.from,
                        &marker.to,
                        &marker.command_id,
                        marker.journal_version,
                        &marker.nonce,
                        "interrupted:commit",
                    )?,
                )?;
            }
        }
        "committed" => {
            let status = status_for(ctx.layout, &marker.command_id, &marker.to, "ok", "");
            write_status_file(ctx.layout, &status)?;
            let _ = remove_request(ctx.layout, &marker.command_id);
            clear_in_progress(ctx.layout)?;
        }
        "rolling_back" => {
            persist_recovery(
                ctx,
                rollback(
                    ctx,
                    &marker.from,
                    &marker.to,
                    &marker.command_id,
                    marker.journal_version,
                    &marker.nonce,
                    "interrupted:rollback",
                )?,
            )?;
        }
        "rollback_refused" => {
            let status = status_for(
                ctx.layout,
                &marker.command_id,
                &marker.to,
                "rollback_refused_journal_migrated",
                "previous cannot read the migrated journal",
            );
            write_status_file(ctx.layout, &status)?;
        }
        _ => clear_in_progress(ctx.layout)?,
    }
    Ok(())
}

fn persist_recovery(ctx: &Apply<'_>, status: Status) -> Result<()> {
    write_status_file(ctx.layout, &status)
}

fn finish(ctx: &Apply<'_>, status: Status) -> Result<Status> {
    write_status_file(ctx.layout, &status)?;
    crash(ctx, "after_status")?;
    let _ = remove_request(ctx.layout, &status.command_id);
    crash(ctx, "after_remove_request")?;
    if !matches!(
        status.result.as_str(),
        "rollback_failed" | "rollback_refused_journal_migrated"
    ) {
        clear_in_progress(ctx.layout)?;
        crash(ctx, "after_clear_in_progress")?;
    }
    Ok(status)
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
    let record = InstallRecord::read(&ctx.layout.on_disk(&ctx.layout.config_path()))?;
    let account = service_account(ctx, &record)?;
    let Some(bytes) = read_service_file(ctx.layout, &ctx.layout.request_path(), &account)? else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_slice(&bytes).context("update request is not valid JSON")?,
    ))
}

fn remove_request(layout: &Layout, command_id: &str) -> Result<()> {
    if command_id.is_empty() {
        return unlink_service_file(layout, &layout.request_path());
    }
    let path = layout.on_disk(&layout.request_path());
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(existing) = serde_json::from_slice::<Request>(&bytes)
        && existing.command_id != command_id
    {
        return Ok(());
    }
    unlink_service_file(layout, &layout.request_path())
}

fn write_status_file(layout: &Layout, status: &Status) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(status)?;
    bytes.push(b'\n');
    write_status(layout, &bytes)?;
    Ok(())
}

struct Marker<'a> {
    from: &'a str,
    to: &'a str,
    phase: &'a str,
    command_id: &'a str,
    journal_version: u32,
    nonce: &'a str,
}

fn write_marker(layout: &Layout, marker: Marker<'_>) -> Result<()> {
    let mut bytes = serde_json::to_vec(&InProgress {
        from: marker.from.to_owned(),
        to: marker.to.to_owned(),
        phase: marker.phase.to_owned(),
        command_id: marker.command_id.to_owned(),
        journal_version: marker.journal_version,
        nonce: marker.nonce.to_owned(),
    })?;
    bytes.push(b'\n');
    write_in_progress(layout, &bytes)?;
    Ok(())
}

fn status_for(layout: &Layout, command_id: &str, release_id: &str, result: &str, error: &str) -> Status {
    Status {
        schema: STATUS_SCHEMA,
        command_id: command_id.to_owned(),
        release_id: release_id.to_owned(),
        result: result.to_owned(),
        error: error.to_owned(),
        running_release_id: current_release(layout).unwrap_or_default(),
    }
}

fn refused(layout: &Layout, request: &Request, error: &str) -> Status {
    status_for(layout, &request.command_id, &request.release_id, "refused", error)
}

fn record_enrolled(layout: &Layout, record: &InstallRecord) -> bool {
    let credentials = layout.on_disk(&layout.state_dir().join("credentials"));
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
    Ok(())
}

fn prepare_attempt(layout: &Layout, release_id: &str, nonce: &str) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "release_id": release_id,
        "nonce": nonce,
    }))?;
    bytes.push(b'\n');
    write_logical(layout, &layout.attempt_path(), &bytes, 0o644)?;
    let _ = unlink_service_file(layout, &layout.health_path());
    Ok(())
}

fn crash(ctx: &Apply<'_>, point: &str) -> Result<()> {
    let env = std::env::var("BOSS_SPOKE_UPDATE_CRASH").ok();
    if env.as_deref() == Some(point) || ctx.crash == Some(point) {
        // Abort the process so the helper cannot run its error-cleanup path.
        // Tests spawn the helper as a subprocess and restart it without this env.
        std::process::abort();
    }
    Ok(())
}

/// Reads SQLite `user_version` through a WAL-aware connection. Missing file is
/// [`None`]; a short or malformed file is an error.
pub fn journal_user_version(layout: &Layout, account: &Account) -> Result<Option<u32>> {
    let Some(path) = journal_sqlite_path(layout, account)? else {
        return Ok(None);
    };
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open {}", path.display()))?;
    conn.pragma_update(None, "query_only", true)
        .with_context(|| format!("cannot open {} read-only", path.display()))?;
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .map(Some)
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
