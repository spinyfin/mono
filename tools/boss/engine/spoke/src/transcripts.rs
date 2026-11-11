use anyhow::{Context, Result};
use boss_engine_driver::{DriverRegistry, ProgressSessionConfig, ProgressStreamSource, discovery};
use boss_host_execution::worker::{ExecutionRecord, Phase};
use boss_protocol_spoke::{
    v1::{Gap, Record, Stamp, TranscriptDelta, record, worker_event::Event},
    worker_event,
};
use boss_spoke_journal::Admission;
use boss_spoke_outbound::SharedJournal;
use boss_transcript_tail::raw;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

// A source checkpoint is limited to 512 KiB. Base64 for 256 KiB uses
// 342 KiB, leaving room for the cursor and normalizer state. Raw transcript
// bytes remain journaled even when this progress-only bound rejects a line.
const MAX_PARTIAL: usize = 256 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct Progress {
    #[serde(with = "partial_bytes")]
    partial: Vec<u8>,
    normalizer: Option<serde_json::Value>,
    /// Raw cursor still advances; JSON assembly waits for the next newline.
    #[serde(default)]
    skip_until_newline: bool,
}

// Byte arrays expand up to fourfold in JSON. Base64 keeps even a partial
// 256 KiB entry within the bounded durable checkpoint, including split UTF-8.
mod partial_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}
#[derive(Default, Serialize, Deserialize)]
struct Checkpoint {
    path: PathBuf,
    cursor: raw::Cursor,
    progress: Progress,
    eof: bool,
    incarnation: u64,
}

/// Stop describes the source observed at the boundary, even when bulk credit
/// is exhausted. Reading only checkpoints assigns no transcript sequence.
pub async fn watermark(
    journal: &SharedJournal,
    execution: &ExecutionRecord,
    path: &std::path::Path,
) -> Result<(String, u64)> {
    let saved = journal
        .lock()
        .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
        .source(&crate::run_identity(execution), "transcript")?;
    let mut checkpoint: Checkpoint = saved
        .map(|b| serde_json::from_slice(&b))
        .transpose()?
        .unwrap_or_default();
    if checkpoint.path != path {
        checkpoint.incarnation += 1;
        checkpoint.cursor = raw::Cursor::default();
    }
    let root = DriverRegistry::default()
        .require(&execution.driver_id)?
        .transcript_containment_root(&execution.run_id)?;
    let chunk = raw::read_contained(path, root.as_deref(), &checkpoint.cursor, 0).await?;
    Ok((
        format!("{}-{}", checkpoint.incarnation, chunk.cursor.generation),
        chunk.observed_size,
    ))
}

#[derive(Debug)]
struct SourceFailure(String);
impl std::fmt::Display for SourceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for SourceFailure {}

/// Consecutive-or-not failed polls of a Terminal run (no pending hooks) with a
/// retryable error before the run is drained with a transcript gap. Without a
/// bound, a persistent unclassified errno (EIO, ESTALE, ...) would block
/// reclaim forever with nothing recorded.
const TERMINAL_TRANSIENT_BUDGET: u64 = 20;
const TRANSIENT_SOURCE: &str = "transient_failures";

pub async fn poll(journal: &SharedJournal, execution: &ExecutionRecord, stamp: &Stamp) -> Result<()> {
    let result = poll_source(journal, execution, stamp).await;
    if let Err(error) = &result
        && execution.phase == Phase::Terminal
        && let Some(observation) = &execution.observation
        && !crate::hooks::has_pending(&observation.pending)?
    {
        let run = crate::run_identity(execution);
        let exhausted = error.downcast_ref::<SourceFailure>().is_some()
            || bump_transient_failures(journal, &run)? >= TERMINAL_TRANSIENT_BUDGET;
        if exhausted {
            let checkpoint = current_checkpoint(journal, &run)?;
            return finish_gap(
                journal,
                &run,
                stamp,
                gap_resource(&checkpoint),
                checkpoint.cursor.offset,
                error.to_string(),
            );
        }
    }
    result
}

/// Durably count a retryable failure against the Terminal budget.
fn bump_transient_failures(journal: &SharedJournal, run: &boss_protocol_spoke::v1::Run) -> Result<u64> {
    let mut journal = journal
        .lock()
        .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?;
    let old = journal.source(run, TRANSIENT_SOURCE)?;
    let count = old
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    journal.commit_source(
        run,
        TRANSIENT_SOURCE,
        old.as_deref(),
        count.to_string().as_bytes(),
        Vec::new(),
    )?;
    Ok(count)
}

async fn poll_source(journal: &SharedJournal, execution: &ExecutionRecord, stamp: &Stamp) -> Result<()> {
    let observation = execution
        .observation
        .as_ref()
        .context("missing observation configuration")?;
    let run = crate::run_identity(execution);
    let (saved, hint, credit) = {
        let journal = journal
            .lock()
            .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?;
        (
            journal.source(&run, "transcript")?,
            journal.latest_source(&run)?,
            journal.bulk_credit(),
        )
    };
    let mut checkpoint: Checkpoint = saved
        .as_ref()
        .map(|b| serde_json::from_slice(b))
        .transpose()?
        .unwrap_or_default();
    let path = if let Some((ingress, baseline)) = &observation.file {
        let ingress = ingress.clone();
        let baseline = baseline.iter().cloned().collect();
        let scan = tokio::task::spawn_blocking(move || {
            let prepared = discovery::PreparedSource::with_baseline(ingress, baseline)?;
            discovery::scan_once_classified(&prepared)
        })
        .await?
        .map_err(|failure| match failure {
            // Retryable I/O (EMFILE, EINTR, ...) in listing, candidate
            // validation or preparation leaves the run undrained so a later
            // poll retries; permanent failures become a source gap.
            discovery::ScanFailure::Transient(message) => anyhow::anyhow!(message),
            discovery::ScanFailure::Permanent(message) => anyhow::Error::from(SourceFailure(message)),
        })?;
        // Keep following an attached file when another candidate appears.
        // Before attachment, ambiguity must never pick a run by recency.
        if scan.matched.iter().any(|c| c.path == checkpoint.path) {
            Some(checkpoint.path.clone())
        } else {
            if scan.matched.len() > 1 {
                return Err(SourceFailure("multiple correlated transcript candidates".into()).into());
            }
            scan.matched.into_iter().next().map(|c| c.path)
        }
    } else {
        hint.map(|bytes| serde_json::from_slice::<crate::hooks::Receipt>(&bytes))
            .transpose()?
            .and_then(|r| r.path)
    };
    let Some(path) = path else {
        if execution.phase == Phase::Terminal && !crate::hooks::has_pending(&observation.pending)? {
            finish_missing(journal, &run, stamp, &checkpoint)?;
        }
        return Ok(());
    };
    if !path.is_absolute() {
        return Err(SourceFailure("transcript path must be absolute".into()).into());
    }
    let mut switched = None;
    if checkpoint.path != path {
        if !checkpoint.path.as_os_str().is_empty() {
            switched = Some(Gap {
                resource: format!("transcript:{}-{}", checkpoint.incarnation, checkpoint.cursor.generation),
                from_offset: checkpoint.cursor.offset,
                through_offset: checkpoint.cursor.offset,
                reason: "transcript path changed before final drain; previous final size unknown".into(),
            });
        }
        checkpoint.incarnation += 1;
        checkpoint.path = path;
        checkpoint.cursor = raw::Cursor::default();
        checkpoint.progress = Progress::default();
        checkpoint.eof = false;
    }
    let root = if let Some((ingress, _)) = &observation.file {
        Some(ingress.directory.clone())
    } else {
        DriverRegistry::default()
            .require(&execution.driver_id)?
            .transcript_containment_root(&execution.run_id)?
    };
    // A missing terminal source is lifecycle evidence even without bulk
    // credit. A zero-byte probe can establish that without advancing a cursor.
    let chunk = match raw::read_contained(
        &checkpoint.path,
        root.as_deref(),
        &checkpoint.cursor,
        credit.as_ref().copied().unwrap_or(0),
    )
    .await
    {
        Ok(chunk) => chunk,
        Err(boss_transcript_tail::TailError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound
                && execution.phase == Phase::Terminal
                && !crate::hooks::has_pending(&observation.pending)? =>
        {
            return finish_missing(journal, &run, stamp, &checkpoint);
        }
        Err(error) => {
            let permanent = match &error {
                boss_transcript_tail::TailError::Io(io) => !discovery::io_error_is_transient(io),
                _ => true,
            };
            return Err(if permanent {
                SourceFailure(error.to_string()).into()
            } else {
                error.into()
            });
        }
    };
    let _ = credit?;
    if chunk.replaced {
        checkpoint.progress = Progress::default();
    }
    let generation = format!("{}-{}", checkpoint.incarnation, chunk.cursor.generation);
    let mut records = Vec::new();
    let make = |body| Record {
        observed: Some(stamp.clone()),
        run: Some(run.clone()),
        body: Some(body),
        ..Default::default()
    };
    if let Some(gap) = switched {
        records.push((make(record::Body::Gap(gap)), Admission::Lifecycle));
    }
    if chunk.replaced {
        records.push((
            make(record::Body::Gap(Gap {
                resource: format!("transcript:{}-{}", checkpoint.incarnation, checkpoint.cursor.generation),
                from_offset: checkpoint.cursor.offset,
                through_offset: chunk.previous_size.max(checkpoint.cursor.offset),
                reason: "source replaced or truncated; any unobserved suffix is unavailable".into(),
            })),
            Admission::Lifecycle,
        ));
    }
    if !chunk.data.is_empty() || !checkpoint.eof || chunk.replaced {
        records.push((
            make(record::Body::Transcript(TranscriptDelta {
                generation: generation.clone(),
                offset: chunk.offset,
                sha256: Sha256::digest(&chunk.data).to_vec(),
                data: chunk.data.clone(),
                eof: chunk.eof,
            })),
            Admission::Bulk,
        ));
    }
    if observation.file.is_some() {
        let driver = DriverRegistry::default().require(&execution.driver_id)?;
        let mut session = driver.progress_session(&ProgressSessionConfig {
            run_id: Some(run.run_id.clone()),
            source: ProgressStreamSource::AgentJsonlFile,
            transcript_path: Some(checkpoint.path.clone()),
            resume_state: checkpoint.progress.normalizer.clone(),
            ..Default::default()
        });
        let normalizer = session.as_mut().context("file driver has no progress normalizer")?;
        if let Some(state) = &checkpoint.progress.normalizer {
            normalizer.restore_resume_state(state).map_err(anyhow::Error::msg)?;
        }
        use boss_engine_stdout_progress::{RawLine, ReaderConfig, ReaderStats, frame_chunk, normalize_line};
        let config = ReaderConfig {
            max_line_bytes: MAX_PARTIAL,
        };
        let frames = frame_chunk(
            &mut checkpoint.progress.partial,
            &mut checkpoint.progress.skip_until_newline,
            &chunk.data,
            execution.phase == Phase::Terminal && chunk.eof,
            config,
        );
        let mut stats = ReaderStats::default();
        for frame in frames {
            if matches!(frame, RawLine::Oversized) {
                records.push((
                    make(record::Body::Gap(Gap {
                        resource: format!("transcript:{generation}:entry"),
                        from_offset: checkpoint.cursor.offset,
                        through_offset: chunk.cursor.offset,
                        reason: "transcript progress entry exceeds 256 KiB checkpoint bound; raw bytes retained".into(),
                    })),
                    Admission::Lifecycle,
                ));
            }
            for envelope in normalize_line(frame, driver.as_ref(), &mut session, config, &mut stats) {
                let mut wire = worker_event::to_wire(&envelope.event);
                if let Some(Event::Stop(stop)) = &mut wire.event {
                    stop.transcript_generation = generation.clone();
                    stop.transcript_through = chunk.cursor.offset;
                }
                records.push((make(record::Body::Event(wire)), Admission::Lifecycle));
            }
        }
        checkpoint.progress.normalizer = session.as_ref().and_then(|session| session.resume_state());
    }
    checkpoint.cursor = chunk.cursor;
    checkpoint.eof = chunk.eof;
    let mut journal = journal
        .lock()
        .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?;
    journal.commit_source(
        &run,
        "transcript",
        saved.as_deref(),
        &serde_json::to_vec(&checkpoint)?,
        records,
    )?;
    if execution.phase == Phase::Terminal
        && checkpoint.eof
        && checkpoint.progress.partial.is_empty()
        && !crate::hooks::has_pending(&observation.pending)?
    {
        journal.commit_source(&run, "drained", None, b"true", Vec::new())?;
    }
    Ok(())
}

fn finish_missing(
    journal: &SharedJournal,
    run: &boss_protocol_spoke::v1::Run,
    stamp: &Stamp,
    checkpoint: &Checkpoint,
) -> Result<()> {
    let reason = if checkpoint.path.as_os_str().is_empty() {
        "terminal run never attached a transcript source"
    } else {
        "attached transcript disappeared before final drain; final size unknown"
    };
    finish_gap(
        journal,
        run,
        stamp,
        gap_resource(checkpoint),
        checkpoint.cursor.offset,
        reason.into(),
    )
}

/// Names the generation that stopped short once a transcript was attached;
/// a plain `transcript` otherwise (never attached).
fn gap_resource(checkpoint: &Checkpoint) -> String {
    if checkpoint.path.as_os_str().is_empty() {
        "transcript".into()
    } else {
        format!("transcript:{}-{}", checkpoint.incarnation, checkpoint.cursor.generation)
    }
}

fn current_checkpoint(journal: &SharedJournal, run: &boss_protocol_spoke::v1::Run) -> Result<Checkpoint> {
    let saved = journal
        .lock()
        .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?
        .source(run, "transcript")?;
    Ok(saved
        .map(|b| serde_json::from_slice(&b))
        .transpose()?
        .unwrap_or_default())
}

fn finish_gap(
    journal: &SharedJournal,
    run: &boss_protocol_spoke::v1::Run,
    stamp: &Stamp,
    resource: String,
    offset: u64,
    reason: String,
) -> Result<()> {
    let mut journal = journal
        .lock()
        .map_err(|_| anyhow::Error::from(boss_spoke_journal::Fault::LockPoisoned))?;
    if journal.source(run, "drained")?.is_some() {
        return Ok(());
    }
    journal.commit_source(
        run,
        "drained",
        None,
        b"true",
        vec![(
            Record {
                observed: Some(stamp.clone()),
                run: Some(run.clone()),
                body: Some(record::Body::Gap(Gap {
                    resource,
                    from_offset: offset,
                    through_offset: offset,
                    reason,
                })),
                ..Default::default()
            },
            Admission::Lifecycle,
        )],
    )?;
    Ok(())
}
