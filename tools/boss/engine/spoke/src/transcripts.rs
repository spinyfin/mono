use anyhow::{Context, Result, ensure};
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

#[derive(Default, Serialize, Deserialize)]
struct Progress {
    #[serde(with = "partial_bytes")]
    partial: Vec<u8>,
    normalizer: Option<serde_json::Value>,
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
        .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
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

pub async fn poll(journal: &SharedJournal, execution: &ExecutionRecord, stamp: &Stamp) -> Result<()> {
    let observation = execution
        .observation
        .as_ref()
        .context("missing observation configuration")?;
    let run = crate::run_identity(execution);
    let (saved, hint, credit) = {
        let journal = journal.lock().map_err(|_| anyhow::anyhow!("journal owner panicked"))?;
        (
            journal.source(&run, "transcript")?,
            journal.latest_source(&run, "hook:")?,
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
            discovery::scan_once(&prepared)
        })
        .await?
        .map_err(anyhow::Error::msg)?;
        // Keep following an attached file when another candidate appears.
        // Before attachment, ambiguity must never pick a run by recency.
        if scan.matched.iter().any(|c| c.path == checkpoint.path) {
            Some(checkpoint.path.clone())
        } else {
            ensure!(scan.matched.len() <= 1, "multiple correlated transcript candidates");
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
    ensure!(path.is_absolute(), "transcript path must be absolute");
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
        Err(error) => return Err(error.into()),
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
        let mut session = driver
            .progress_session(&ProgressSessionConfig {
                run_id: Some(run.run_id.clone()),
                source: ProgressStreamSource::AgentJsonlFile,
                transcript_path: Some(checkpoint.path.clone()),
                resume_state: checkpoint.progress.normalizer.clone(),
                ..Default::default()
            })
            .context("file driver has no progress normalizer")?;
        if let Some(state) = &checkpoint.progress.normalizer {
            session.restore_resume_state(state).map_err(anyhow::Error::msg)?;
        }
        checkpoint.progress.partial.extend_from_slice(&chunk.data);
        if execution.phase == Phase::Terminal
            && chunk.eof
            && !checkpoint.progress.partial.is_empty()
            && checkpoint.progress.partial.last() != Some(&b'\n')
        {
            checkpoint.progress.partial.push(b'\n');
        }
        let mut consumed = 0;
        for (index, byte) in checkpoint.progress.partial.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let line = &checkpoint.progress.partial[consumed..index];
            consumed = index + 1;
            let value: serde_json::Value = match serde_json::from_slice(line) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(run = run.run_id, %error, "invalid transcript JSON retained in raw stream");
                    continue;
                }
            };
            match session.normalize_progress_events(&value) {
                Ok(events) => {
                    for event in events {
                        let mut wire = worker_event::to_wire(&event);
                        if let Some(Event::Stop(stop)) = &mut wire.event {
                            stop.transcript_generation = generation.clone();
                            stop.transcript_through = chunk.cursor.offset;
                        }
                        records.push((make(record::Body::Event(wire)), Admission::Lifecycle));
                    }
                }
                // Match the shared JSONL reader: driver-rejected records do
                // not stop the stream, and their original bytes remain evidence.
                Err(error) => tracing::debug!(run = run.run_id, %error, "driver skipped transcript entry"),
            }
        }
        checkpoint.progress.partial.drain(..consumed);
        ensure!(
            checkpoint.progress.partial.len() <= 256 * 1024,
            "unterminated transcript entry exceeds limit"
        );
        checkpoint.progress.normalizer = session.resume_state();
    }
    checkpoint.cursor = chunk.cursor;
    checkpoint.eof = chunk.eof;
    let mut journal = journal.lock().map_err(|_| anyhow::anyhow!("journal owner panicked"))?;
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
    let records = if checkpoint.path.as_os_str().is_empty() {
        Vec::new()
    } else {
        vec![(
            Record {
                observed: Some(stamp.clone()),
                run: Some(run.clone()),
                body: Some(record::Body::Gap(Gap {
                    resource: format!("transcript:{}-{}", checkpoint.incarnation, checkpoint.cursor.generation),
                    from_offset: checkpoint.cursor.offset,
                    through_offset: checkpoint.cursor.offset,
                    reason: "attached transcript disappeared before final drain; final size unknown".into(),
                })),
                ..Default::default()
            },
            Admission::Lifecycle,
        )]
    };
    journal
        .lock()
        .map_err(|_| anyhow::anyhow!("journal owner panicked"))?
        .commit_source(run, "drained", None, b"true", records)?;
    Ok(())
}
