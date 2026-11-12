use anyhow::{Context, Result, ensure};
use boss_engine_driver::DriverRegistry;
use boss_host_execution::worker::ExecutionRecord;
use boss_protocol_spoke::{
    v1::{Gap, Record, Stamp, record},
    worker_event,
};
use boss_spoke_journal::Admission;
use boss_spoke_outbound::SharedJournal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

const MAX_HOOK: usize = 256 * 1024;
#[derive(Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub digest: String,
    pub path: Option<PathBuf>,
}

fn digest_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
        out
    })
}

pub use boss_spoke_worker_api::authenticate;

pub async fn ingest(journal: &SharedJournal, execution: &ExecutionRecord, bytes: &[u8], stamp: &Stamp) -> Result<()> {
    ensure!(bytes.len() <= MAX_HOOK, "hook exceeds payload limit");
    let raw: serde_json::Value = serde_json::from_slice(bytes)?;
    let run = crate::run_identity(execution);
    ensure!(
        raw["_boss_run_id"].as_str() == Some(&run.run_id),
        "hook run identity mismatch"
    );
    let id = raw["_boss_producer_id"].as_str().context("missing producer identity")?;
    ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid producer identity"
    );
    let sequence = raw["_boss_producer_sequence"]
        .as_u64()
        .filter(|s| *s > 0)
        .context("invalid producer sequence")?;
    let source = format!("hook:{id}:{sequence}");
    let digest = digest_hex(&Sha256::digest(bytes));
    let previous = {
        let journal = journal
            .lock()
            .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?;
        if let Some(old) = journal.source(&run, &source)? {
            ensure!(
                serde_json::from_slice::<Receipt>(&old)?.digest == digest,
                "producer identity reused with different bytes"
            );
            return Ok(());
        }
        journal.latest_source(&run)?
    };
    let driver = DriverRegistry::default().require(&execution.driver_id)?;
    let previous: Option<Receipt> = previous.map(|bytes| serde_json::from_slice(&bytes)).transpose()?;
    let subagent = raw["agent_id"].as_str().is_some_and(|id| !id.is_empty());
    let path = if subagent {
        None
    } else {
        driver.transcript_path_for_session(&raw).map(PathBuf::from)
    }
    .or_else(|| previous.and_then(|r| r.path));
    let mut observed = stamp.clone();
    if let Some(wall) = raw["_boss_observed_unix_ms"].as_i64().filter(|wall| *wall > 0) {
        observed.wall_unix_ms = wall;
    }
    let file_ingress = execution.observation.as_ref().is_some_and(|o| o.file.is_some());
    // The driver's selected source is authoritative. Codex hooks carry policy
    // observations; its rollout is the progress feed, so never emit both.
    let observations = if file_ingress {
        Vec::new()
    } else {
        let event = driver.normalize_progress_event(&raw)?;
        let mut wire = worker_event::to_wire(&event);
        wire.from_subagent = subagent;
        let mut observations = Vec::new();
        if let Some(boss_protocol_spoke::v1::worker_event::Event::Stop(stop)) = &mut wire.event
            && !subagent
            && let Some(path) = &path
        {
            match crate::transcripts::watermark(journal, execution, path).await {
                Ok((generation, through)) => {
                    stop.transcript_generation = generation;
                    stop.transcript_through = through;
                }
                Err(error) => observations.push((
                    Record {
                        observed: Some(observed.clone()),
                        run: Some(run.clone()),
                        body: Some(record::Body::Gap(Gap {
                            resource: "transcript".into(),
                            reason: format!("transcript unavailable at Stop: {error}"),
                            ..Default::default()
                        })),
                        ..Default::default()
                    },
                    Admission::Lifecycle,
                )),
            }
        }
        if raw["_boss_payload_truncated"].as_bool() == Some(true) {
            observations.push((
                Record {
                    observed: Some(observed.clone()),
                    run: Some(run.clone()),
                    body: Some(record::Body::Gap(Gap {
                        resource: format!("hook:{id}:{sequence}"),
                        reason:
                            "oversized hook payload truncated by the forwarder; fields beyond the bound are unavailable"
                                .into(),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                Admission::Lifecycle,
            ));
        }
        observations.push((
            Record {
                observed: Some(observed),
                run: Some(run.clone()),
                body: Some(record::Body::Event(wire)),
                ..Default::default()
            },
            Admission::Lifecycle,
        ));
        observations
    };
    let turn_stop = !subagent && raw["hook_event_name"].as_str() == Some("Stop");
    if turn_stop {
        boss_spoke_worker_api::note_turn_stop(journal, &run)?;
    }
    let mut journal = journal
        .lock()
        .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?;
    if let Some(old) = journal.source(&run, &source)? {
        ensure!(
            serde_json::from_slice::<Receipt>(&old)?.digest == digest,
            "producer identity reused with different bytes"
        );
        return Ok(());
    }
    journal.commit_source(
        &run,
        &source,
        None,
        &serde_json::to_vec(&Receipt { digest, path })?,
        observations,
    )?;
    drop(journal);
    Ok(())
}

pub async fn drain(journal: &SharedJournal, execution: &ExecutionRecord, stamp: &Stamp) -> Result<()> {
    use fs4::fs_std::FileExt;
    let pending = &execution
        .observation
        .as_ref()
        .context("missing observation configuration")?
        .pending;
    let metadata = std::fs::symlink_metadata(pending)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "unsafe hook spool"
    );
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(pending.join("producer.lock"))?;
    if !FileExt::try_lock_exclusive(&lock)? {
        return Ok(());
    }
    // Bound memory and work while selecting the oldest producer sequences.
    let mut paths = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(pending)? {
        let path = entry?.path();
        // A crash after fsync but before rename leaves a complete .tmp. It
        // carries the same producer identity and is safe to deduplicate too.
        if matches!(path.extension().and_then(|s| s.to_str()), Some("json" | "tmp")) {
            paths.insert(path);
            if paths.len() > 64 {
                paths.pop_last();
            }
        }
    }
    for path in paths {
        match ingest_pending(journal, execution, stamp, &path).await {
            Ok(()) => {
                if let Err(error) = std::fs::remove_file(&path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(error.into());
                }
                std::fs::File::open(pending)?.sync_all()?;
            }
            Err(error) if boss_spoke_journal::Fault::is_retryable(&error) => return Err(error),
            Err(error) => {
                tracing::warn!(
                    run = execution.run_id,
                    path = %path.display(),
                    %error,
                    "quarantining undecodable pending hook"
                );
                quarantine_pending(journal, execution, stamp, pending, &path, &error)?;
            }
        }
    }
    Ok(())
}

async fn ingest_pending(
    journal: &SharedJournal,
    execution: &ExecutionRecord,
    stamp: &Stamp,
    path: &Path,
) -> Result<()> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => anyhow::bail!("pending hook file unreadable: {error}"),
    };
    let metadata = file
        .metadata()
        .map_err(|error| anyhow::anyhow!("pending hook file unreadable: {error}"))?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.permissions().mode() & 0o077 == 0,
        "unsafe pending hook file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_HOOK as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| anyhow::anyhow!("pending hook file unreadable: {error}"))?;
    ingest(journal, execution, &bytes, stamp).await
}

fn quarantine_pending(
    journal: &SharedJournal,
    execution: &ExecutionRecord,
    stamp: &Stamp,
    pending: &Path,
    path: &Path,
    error: &anyhow::Error,
) -> Result<()> {
    let name = path
        .file_name()
        .context("pending hook file name")?
        .to_string_lossy()
        .into_owned();
    let resource = format!("pending:{name}");
    let run = crate::run_identity(execution);
    let records = vec![(
        Record {
            observed: Some(stamp.clone()),
            run: Some(run.clone()),
            body: Some(record::Body::Gap(Gap {
                resource: resource.clone(),
                reason: format!("undecodable pending hook: {error}"),
                ..Default::default()
            })),
            ..Default::default()
        },
        Admission::Lifecycle,
    )];
    {
        let mut journal = journal
            .lock()
            .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?;
        if journal.source(&run, &resource)?.is_none() {
            journal.commit_source(&run, &resource, None, b"quarantined", records)?;
        }
    }
    let quarantine = pending.join("quarantine");
    std::fs::create_dir_all(&quarantine)?;
    std::fs::set_permissions(&quarantine, std::fs::Permissions::from_mode(0o700))?;
    if let Err(move_error) = std::fs::rename(path, quarantine.join(&name))
        && move_error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(move_error.into());
    }
    std::fs::File::open(pending)?.sync_all()?;
    Ok(())
}

pub fn has_pending(directory: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(directory)? {
        if matches!(entry?.path().extension().and_then(|s| s.to_str()), Some("json" | "tmp")) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        ensure!(
            std::os::unix::net::UnixStream::connect(path).is_err(),
            "hook socket already active"
        );
        use std::os::unix::fs::FileTypeExt;
        ensure!(
            std::fs::symlink_metadata(path)?.file_type().is_socket(),
            "hook socket path is not a socket"
        );
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn serve(listener: UnixListener, journal: SharedJournal, stamp: impl Fn() -> Result<Stamp>) -> Result<()> {
    let mut failures = 0u32;
    loop {
        let mut stream = match listener.accept().await {
            Ok((stream, _)) => {
                failures = 0;
                stream
            }
            Err(error) => {
                tracing::warn!(%error, "hook accept failed");
                let delay = if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) {
                    50
                } else {
                    (10u64 << failures.min(6)).min(500)
                };
                failures = failures.saturating_add(1);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                continue;
            }
        };
        // Read and attribute before accepting another peer; bounded deadline
        // and bytes keep an idle or oversized client from pinning ingress.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receive(&mut stream, &journal, &stamp()?),
        )
        .await;
        let response = match result {
            Ok(Ok(())) => "BOSS-EVENT-RECORDED\n".to_owned(),
            Ok(Err(error)) => format!("BOSS-EVENT-RETRY: {error}\n"),
            Err(_) => "BOSS-EVENT-RETRY: read deadline\n".into(),
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), stream.write_all(response.as_bytes())).await;
    }
}

async fn receive(stream: &mut UnixStream, journal: &SharedJournal, stamp: &Stamp) -> Result<()> {
    let credentials = stream.peer_cred()?;
    let execution = authenticate(
        &crate::registered(journal)?,
        credentials.pid().context("missing peer pid")?,
        credentials.uid(),
        &stamp.boot_id,
    )?;
    let mut bytes = Vec::new();
    stream.take(MAX_HOOK as u64 + 1).read_to_end(&mut bytes).await?;
    drain(journal, &execution, stamp).await?;
    ensure!(
        !has_pending(
            &execution
                .observation
                .as_ref()
                .context("missing observation configuration")?
                .pending
        )?,
        "older pending observations remain"
    );
    ingest(journal, &execution, &bytes, stamp).await
}
