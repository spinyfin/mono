//! Restricted worker IPC. Durable receipt never confers hub work authority.
//!
//! Command inventory: `execution-runner/src/runner/prompt.rs`, `prompt/`,
//! `prompt_fragments.rs`, `review_prompt.rs`, `review_guide_prompt.rs` and
//! `progression_prompt.rs` compose context, PR status/body, attachment,
//! proposal and guide-outcome instructions. `worker-api/1` accepts those bound
//! reads, attachment registration/listing, the existing validated proposal
//! kinds (including done, wait, review reports/verdicts, blocked questions,
//! followups and PR-created) and `boss comment guide-outcome`, a run-scoped
//! mutation queued as a validated `guide-outcome/1` record bound to the
//! claimed run. Listing returns this assignment's local receipts, explicitly
//! labeled as such. Context uses Spawn's sanitized bundle and context
//! revision. Cache misses and explicit refreshes emit WorkerRequest on the
//! journal stream; absence of a reply is hub_unavailable. Cached reads retain
//! observation time.
//!
//! Prompt paths for progression supervision also mention arbitrary task reads
//! (`task show`), design-doc commands (`project design-doc`,
//! `project set-design-doc`), stuck reports (`propose unreachable`) and
//! merge/CI recovery (`resolved merge`, `engine ci`), which are engine
//! administration verbs. These are denied, as are every general
//! app/board/control method. Enabling such execution kinds remotely requires
//! their own run-scoped contracts.
//! Cube publication and gh remain host-local commands using host credentials.
//! The host-allocated PR/structured-output files are captured into durable
//! artifact sources and chunked over the same stream, including after restart.
//!
//! Spool lifecycle: staged artifact bytes move into numbered records chunk by
//! chunk in one transaction, so they are never counted twice. Request/reply
//! rows are deleted once consumed, and every remaining `worker-*` row is
//! released when the run is reclaimed and its artifacts are fully streamed.
//!
//! Proposal receipts permit a worker to finish its turn and park. Decisions
//! arrive separately through ProposalDecision; only hub commands may stop or
//! release an assignment. No completion or scheduling policy lives here.
mod artifacts {
    use super::*;
    use boss_engine_attachments::{AllowedRoots, AttachmentStore};
    use sha2::{Digest, Sha256};
    use std::{io::Read, os::unix::fs::OpenOptionsExt, path::Path};

    const CHUNK: usize = 64 * 1024;
    /// Chunks emitted per run in one observe tick, while bulk credit lasts.
    const PUMP_BURST: usize = 8;

    #[derive(Serialize, Deserialize)]
    pub(super) struct Artifact {
        id: String,
        kind: i32,
        size: usize,
        metadata: Value,
        offset: usize,
    }

    fn blob_key(id: &str, index: usize) -> String {
        format!("worker-blob:{id}:{index}")
    }

    /// How one host-allocated output file resolved at a capture boundary.
    enum Captured {
        /// Registered, or already registered by an earlier attempt.
        Registered,
        /// Nothing to capture (yet).
        Absent,
        /// Retry later: the spool had no room or the journal faulted.
        Transient(anyhow::Error),
        /// Will never become valid; the hub gets an explicit marker instead.
        Invalid(String),
    }

    impl Gateway {
        pub(super) fn attach(
            &self,
            execution: &ExecutionRecord,
            path: &str,
            caption: Option<String>,
            stamp: &v1::Stamp,
        ) -> Result<FrontendEvent> {
            let caption = match boss_engine_attachments::store::validate_caption(caption.as_deref()) {
                Ok(caption) => caption,
                Err(error) => return Ok(FrontendEvent::ProposalRejected { error }),
            };
            let workspace = execution
                .observation
                .as_ref()
                .context("missing workspace")?
                .workspace
                .as_path();
            let store = AttachmentStore::at(self.state_dir.join("attachment-staging"));
            let image = match store.ingest(Path::new(path), &AllowedRoots::for_workspace(Some(workspace))) {
                Ok(image) => image,
                Err(error) => {
                    return Ok(FrontendEvent::ProposalRejected {
                        error: boss_protocol::ProposalSubmissionError::validation(vec![error.into_field_error()]),
                    });
                }
            };
            let bytes = store
                .read_blob(&image.content_digest, image.media_type)?
                .context("staged attachment missing")?;
            let result = self.register(
                execution,
                v1::ArtifactKind::Attachment,
                &bytes,
                json!({
                    "caption": caption, "source_name": image.source_name, "media_type": image.media_type,
                    "pixel_width": image.pixel_width, "pixel_height": image.pixel_height,
                }),
                stamp,
            );
            // The journal owns the bytes after registration. Never remove the worker's source.
            store.remove_blob(&image.content_digest, image.media_type)?;
            result
        }

        fn register(
            &self,
            execution: &ExecutionRecord,
            kind: v1::ArtifactKind,
            bytes: &[u8],
            mut metadata: Value,
            stamp: &v1::Stamp,
        ) -> Result<FrontendEvent> {
            ensure!(
                !bytes.is_empty() && bytes.len() as u64 <= boss_protocol::ATTACHMENT_MAX_BYTES,
                "artifact size exceeds limit"
            );
            let digest = boss_engine_attachments::store::digest_hex(bytes);
            let id = format!("{}:{digest}", kind as i32);
            let run = identity(execution);
            let mut journal = lock(&self.journal)?;
            let (old, mut state) = state(&journal, &run)?;
            if let Some(artifact) = state.artifacts.iter().find(|a| a.id == id) {
                return Ok(response(
                    "queued",
                    json!({"artifact": artifact, "already_registered": true}),
                ));
            }
            ensure!(
                state.artifacts.iter().filter(|a| a.kind == kind as i32).count()
                    < boss_protocol::ATTACHMENT_CAP_PER_EXECUTION,
                "artifact kind per-run cap reached"
            );
            // Staging is spool-accounted bulk data, without allocating outbound
            // sequences ahead of lifecycle records. The pump moves each staged
            // chunk into a record atomically, so the footprint never doubles.
            // A failed registration releases whatever it staged.
            let mut staged = Vec::new();
            for (index, chunk) in bytes.chunks(CHUNK).enumerate() {
                let key = blob_key(&id, index);
                if let Err(error) = journal.commit_source(&run, &key, None, chunk, vec![]) {
                    let _ = journal.delete_sources(&run, &staged);
                    return Err(error);
                }
                staged.push(key);
            }
            metadata["registered_unix_ms"] = stamp.wall_unix_ms.into();
            state.artifacts.push(Artifact {
                id,
                kind: kind as i32,
                size: bytes.len(),
                metadata,
                offset: 0,
            });
            let committed = serde_json::to_vec(&state)
                .map_err(anyhow::Error::from)
                .and_then(|encoded| journal.commit_source(&run, STATE, old.as_deref(), &encoded, vec![]));
            if let Err(error) = committed {
                let _ = journal.delete_sources(&run, &staged);
                return Err(error);
            }
            Ok(response(
                "queued",
                json!({"artifact": state.artifacts.last(), "already_registered": false}),
            ))
        }

        fn read_output(&self, execution: &ExecutionRecord, name: &str, path: &str, stamp: &v1::Stamp) -> Captured {
            let kind = match name {
                "BOSS_PR_URL_OUTPUT" => v1::ArtifactKind::PrUrl,
                _ => v1::ArtifactKind::ReviewResult,
            };
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Captured::Absent,
                Err(error) => return Captured::Invalid(format!("output unreadable: {error}")),
            };
            match file.metadata() {
                Ok(meta) if meta.is_file() => {}
                Ok(_) => return Captured::Invalid("output is not a regular file".into()),
                Err(error) => return Captured::Invalid(format!("output unreadable: {error}")),
            }
            let mut bytes = Vec::new();
            if let Err(error) = file
                .take(boss_protocol::ATTACHMENT_MAX_BYTES + 1)
                .read_to_end(&mut bytes)
            {
                return Captured::Invalid(format!("output unreadable: {error}"));
            }
            if bytes.is_empty() {
                return Captured::Absent;
            }
            if bytes.len() as u64 > boss_protocol::ATTACHMENT_MAX_BYTES {
                return Captured::Invalid("output exceeds limit".into());
            }
            let value: Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(error) => return Captured::Invalid(format!("output is not JSON: {error}")),
            };
            if kind == v1::ArtifactKind::PrUrl
                && let Err(errors) =
                    boss_engine_proposal_validation::validate_payload(boss_protocol::ProposalKind::PrCreated, &value)
            {
                return Captured::Invalid(format!("invalid PR output: {errors:?}"));
            }
            match self.register(
                execution,
                kind,
                &bytes,
                json!({"output_kind": execution.worker_outputs.get("BOSS_STRUCTURED_OUTPUT_KIND"), "source": name}),
                stamp,
            ) {
                Ok(_) => Captured::Registered,
                Err(error) if error.to_string().contains("per-run cap") => Captured::Invalid(error.to_string()),
                Err(error) => Captured::Transient(error),
            }
        }

        /// Record that an output name has reached its final outcome, so it is
        /// captured once: one slot per output name, never one per rewrite. An
        /// invalid output also leaves a lifecycle gap the hub can classify.
        fn conclude_output(
            &self,
            execution: &ExecutionRecord,
            name: &str,
            gap: Option<String>,
            stamp: &v1::Stamp,
        ) -> Result<()> {
            let run = identity(execution);
            let mut journal = lock(&self.journal)?;
            let (old, mut state) = state(&journal, &run)?;
            if state.captured.iter().any(|n| n == name) {
                return Ok(());
            }
            state.captured.push(name.to_owned());
            let observations = gap
                .map(|reason| {
                    vec![(
                        v1::Record {
                            run: Some(run.clone()),
                            observed: Some(stamp.clone()),
                            body: Some(record::Body::Gap(v1::Gap {
                                resource: format!("output:{name}"),
                                reason,
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                        Admission::Lifecycle,
                    )]
                })
                .unwrap_or_default();
            journal.commit_source(&run, STATE, old.as_deref(), &serde_json::to_vec(&state)?, observations)
        }

        /// Capture host-allocated output paths recorded before launching the
        /// worker. Worker-supplied paths and symlinks never become output
        /// authority. Called at completion boundaries (done, stop, exit), so a
        /// file rewritten while the worker runs is one artifact, not many.
        /// Output problems never fail the caller: they become retries or an
        /// explicit gap marker. Only journal faults are returned.
        pub fn capture_outputs(&self, execution: &ExecutionRecord, stamp: &v1::Stamp) -> Result<()> {
            let run = identity(execution);
            for (name, path) in &execution.worker_outputs {
                if !matches!(name.as_str(), "BOSS_PR_URL_OUTPUT" | "BOSS_STRUCTURED_OUTPUT") {
                    continue;
                }
                if state(&*lock(&self.journal)?, &run)?.1.captured.contains(name) {
                    continue;
                }
                match self.read_output(execution, name, path, stamp) {
                    Captured::Registered => self.conclude_output(execution, name, None, stamp)?,
                    Captured::Absent => {}
                    Captured::Transient(error) => {
                        tracing::warn!(run = execution.run_id, output = name, %error, "worker output retained for retry");
                    }
                    Captured::Invalid(reason) => self.conclude_output(execution, name, Some(reason), stamp)?,
                }
            }
            Ok(())
        }

        /// Emit credited chunks for the run's first unfinished artifact, a
        /// bounded burst per call. Each chunk's source cursor commits atomically
        /// with the record, and a fully emitted staged chunk is released in the
        /// same transaction. Restart never duplicates a chunk or loses EOF.
        pub fn pump(&self, run: &v1::Run, stamp: &v1::Stamp) -> Result<()> {
            for burst in 0..PUMP_BURST {
                match self.pump_one(run, stamp) {
                    Ok(true) => {}
                    Ok(false) => return Ok(()),
                    Err(error) if burst > 0 && boss_spoke_journal::Fault::is_full(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }

        fn pump_one(&self, run: &v1::Run, stamp: &v1::Stamp) -> Result<bool> {
            let mut journal = lock(&self.journal)?;
            let (old, mut state) = state(&journal, run)?;
            let Some(artifact) = state.artifacts.iter_mut().find(|a| a.offset < a.size) else {
                return Ok(false);
            };
            let credit = journal.bulk_credit()?;
            let chunk_index = artifact.offset / CHUNK;
            let key = blob_key(&artifact.id, chunk_index);
            let source = journal.source(run, &key)?.context("artifact source missing")?;
            let start = artifact.offset % CHUNK;
            let data = source[start..].iter().copied().take(credit).collect::<Vec<_>>();
            let chunk = v1::ArtifactChunk {
                artifact_id: artifact.id.clone(),
                kind: artifact.kind,
                offset: artifact.offset as u64,
                sha256: Sha256::digest(&data).to_vec(),
                eof: artifact.offset + data.len() == artifact.size,
                total_bytes: artifact.size as u64,
                metadata_json: serde_json::to_vec(&artifact.metadata)?,
                data,
            };
            artifact.offset += chunk.data.len();
            let released = if artifact.offset % CHUNK == 0 || artifact.offset == artifact.size {
                vec![key]
            } else {
                vec![]
            };
            journal.commit_source_releasing(
                run,
                STATE,
                old.as_deref(),
                &serde_json::to_vec(&state)?,
                vec![(
                    v1::Record {
                        run: Some(run.clone()),
                        observed: Some(stamp.clone()),
                        body: Some(record::Body::Artifact(chunk)),
                        ..Default::default()
                    },
                    Admission::Bulk,
                )],
                &released,
            )?;
            Ok(true)
        }

        /// Whether every registered artifact has been fully emitted as records.
        fn fully_streamed(&self, run: &v1::Run) -> Result<bool> {
            Ok(state(&*lock(&self.journal)?, run)?
                .1
                .artifacts
                .iter()
                .all(|a| a.offset >= a.size))
        }

        /// Release a reclaimed run's gateway rows once nothing is left to stream.
        pub fn release_if_done(&self, run: &v1::Run) -> Result<()> {
            if self.fully_streamed(run)? {
                lock(&self.journal)?.release_worker_sources(run)?;
            }
            Ok(())
        }
    }
}

use anyhow::{Context, Result, ensure};
use boss_host_execution::worker::{ExecutionRecord, Phase};
use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope};
use boss_protocol_spoke::{
    Validate,
    v1::{self, command, record},
};
use boss_spoke_journal::{Admission, Journal};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};

pub type SharedJournal = Arc<Mutex<Journal>>;
/// The transcript source as it is now: `(generation, observed size)`.
pub type ProbeFuture = Pin<Box<dyn Future<Output = Result<(String, u64)>> + Send>>;
/// Supplied by the daemon, which owns transcript discovery. A completion
/// boundary records what this reports, independent of how much has been
/// ingested or of bulk credit.
pub type TranscriptProbe = Arc<dyn Fn(ExecutionRecord) -> ProbeFuture + Send + Sync>;
const MAX_REQUEST: u64 = 256 * 1024;
const STATE: &str = "worker-api";
/// Concurrent worker exchanges; further clients wait in the listen backlog.
const MAX_CONNECTIONS: usize = 16;
const GUIDE_OUTCOME_CONTRACT: &str = "guide-outcome/1";
const GUIDE_OUTCOME_KIND: &str = "guide_outcome";
const MAX_GUIDE_BODY_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct Gateway {
    pub journal: SharedJournal,
    pub state_dir: PathBuf,
    pub probe: TranscriptProbe,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    proposals: Vec<Receipt>,
    artifacts: Vec<artifacts::Artifact>,
    /// Output names whose capture reached a final outcome.
    #[serde(default)]
    captured: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Cached {
    revision: String,
    observed_unix_ms: i64,
    event: FrontendEvent,
}

pub fn identity(record: &ExecutionRecord) -> v1::Run {
    v1::Run {
        execution_id: record.execution_id.clone(),
        run_id: record.run_id.clone(),
        spawn_token: record.spawn_token.clone(),
        assignment_generation: record.assignment_generation,
    }
}

fn lock(journal: &SharedJournal) -> Result<std::sync::MutexGuard<'_, Journal>> {
    journal
        .lock()
        .map_err(|_| boss_spoke_journal::Fault::LockPoisoned.into())
}

fn state(journal: &Journal, run: &v1::Run) -> Result<(Option<Vec<u8>>, State)> {
    let bytes = journal.source(run, STATE)?;
    let state = bytes
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()?
        .unwrap_or_default();
    Ok((bytes, state))
}

fn response(status: &str, value: Value) -> FrontendEvent {
    FrontendEvent::WorkerGatewayResult {
        status: status.into(),
        value,
    }
}

pub fn authenticate(records: &[ExecutionRecord], peer: i32, uid: u32, boot: &str) -> Result<ExecutionRecord> {
    // SAFETY: geteuid has no preconditions.
    ensure!(uid == unsafe { libc::geteuid() }, "foreign worker user");
    let mut pid = peer;
    for _ in 0..64 {
        let process = boss_host_execution::process::process_snapshot(pid)?;
        for record in records {
            if record.boot_id == boot
                && record.phase != Phase::Terminal
                && record.pid == Some(pid as u64)
                && record.observation.as_ref().and_then(|o| o.pid_start) == Some(process.start)
            {
                return Ok(record.clone());
            }
        }
        let Some(parent) = process.ppid.filter(|p| *p > 1 && *p != pid) else {
            break;
        };
        pid = parent;
    }
    anyhow::bail!("worker peer is not descended from an assigned worker")
}

impl Gateway {
    /// Per-second upkeep. One bad row or one transient fault is logged and
    /// skipped; only a poisoned journal lock ends the loop.
    pub async fn observe(&self, stamp: impl Fn() -> Result<v1::Stamp>) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let rows = match lock(&self.journal)?.list_executions() {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(%error, "worker gateway could not list executions");
                    continue;
                }
            };
            let stamp = match stamp() {
                Ok(stamp) => stamp,
                Err(error) => {
                    tracing::warn!(%error, "worker gateway upkeep skipped: no clock stamp");
                    continue;
                }
            };
            for bytes in rows {
                let execution: ExecutionRecord = match serde_json::from_slice(&bytes) {
                    Ok(execution) => execution,
                    Err(error) => {
                        tracing::warn!(%error, "worker gateway skipped an undecodable execution row");
                        continue;
                    }
                };
                let run = identity(&execution);
                // Outputs are captured at exit, never while the worker may still
                // rewrite them, and never for a run whose rows were released.
                if matches!(execution.phase, Phase::Stopping | Phase::Terminal)
                    && !execution.reclaimed
                    && let Err(error) = self.capture_outputs(&execution, &stamp)
                {
                    tracing::warn!(run = execution.run_id, %error, "worker output capture failed");
                }
                if let Err(error) = self.pump(&run, &stamp)
                    && !boss_spoke_journal::Fault::is_full(&error)
                {
                    tracing::warn!(run = execution.run_id, %error, "worker artifact retained for retry");
                }
                if execution.reclaimed
                    && let Err(error) = self.release_if_done(&run)
                {
                    tracing::warn!(run = execution.run_id, %error, "worker gateway release retained for retry");
                }
            }
        }
    }

    /// Commit only the assigned snapshot; all later reads retain its revision.
    pub fn provision(&self, command: &v1::Command, stamp: &v1::Stamp) -> Result<()> {
        let Some(command::Body::Spawn(spawn)) = &command.body else {
            return Ok(());
        };
        if spawn.run_context_json.is_empty() {
            return Ok(());
        }
        let run = command.run.as_ref().context("missing assignment")?;
        ensure!(!spawn.context_revision.is_empty(), "context revision missing");
        let bundle = serde_json::from_slice(&spawn.run_context_json)?;
        let cached = Cached {
            revision: spawn.context_revision.clone(),
            observed_unix_ms: stamp.wall_unix_ms,
            event: FrontendEvent::WorkerContextResult { bundle },
        };
        let mut journal = lock(&self.journal)?;
        if journal.source(run, "worker-cache:1")?.is_none() {
            journal.commit_source(run, "worker-cache:1", None, &serde_json::to_vec(&cached)?, vec![])?;
        }
        Ok(())
    }

    /// Called by the ordinary command executor. Replay is a compare-and-swap
    /// against the same run and request, independent of the network receipt.
    pub fn apply(&self, command: &v1::Command) -> Result<()> {
        command.validate()?;
        let run = command.run.as_ref().context("missing assignment")?;
        let mut journal = lock(&self.journal)?;
        let assigned: ExecutionRecord = serde_json::from_slice(
            &journal
                .load_execution(&run.run_id, &run.spawn_token)?
                .context("unknown assignment")?,
        )?;
        ensure!(identity(&assigned) == *run, "assignment generation mismatch");
        match command.body.as_ref().context("missing command")? {
            command::Body::WorkerReply(reply) => {
                let key = format!("worker-request:{}", reply.request_id);
                // A request is deleted once its reply is consumed or its wait
                // timed out, so a replayed or late reply has nothing to update.
                let Some(request) = journal.source(run, &key)? else {
                    tracing::debug!(request = reply.request_id, "worker reply for a closed request ignored");
                    return Ok(());
                };
                let method: i32 = serde_json::from_slice(&request)?;
                let source = format!("worker-reply:{}", reply.request_id);
                let bytes = serde_json::to_vec(&json!({"code": reply.code, "reason": reply.reason,
                    "revision": reply.revision, "observed_unix_ms": reply.observed_unix_ms,
                    "result": serde_json::from_slice::<Value>(&reply.result_json).unwrap_or(Value::Null)}))?;
                if let Some(old) = journal.source(run, &source)? {
                    ensure!(old == bytes, "worker reply changed");
                    return Ok(());
                }
                if reply.code == v1::ResultCode::Ok as i32 {
                    ensure!(
                        !reply.revision.is_empty() && reply.observed_unix_ms > 0,
                        "reply has no observation revision/time"
                    );
                    let event: FrontendEvent = serde_json::from_slice(&reply.result_json)?;
                    ensure!(
                        matches!(
                            (v1::WorkerMethod::try_from(method)?, &event),
                            (v1::WorkerMethod::RunContext, FrontendEvent::WorkerContextResult { .. })
                                | (v1::WorkerMethod::PrStatus, FrontendEvent::PrStatusResult { .. })
                                | (v1::WorkerMethod::PrBody, FrontendEvent::PrBodyResult { .. })
                        ),
                        "reply method mismatch"
                    );
                    let cache_key = format!("worker-cache:{method}");
                    let old = journal.source(run, &cache_key)?;
                    let cached = Cached {
                        event,
                        revision: reply.revision.clone(),
                        observed_unix_ms: reply.observed_unix_ms,
                    };
                    journal.commit_source(run, &cache_key, old.as_deref(), &serde_json::to_vec(&cached)?, vec![])?;
                }
                let old = journal.source(run, &source)?;
                ensure!(old.as_ref().is_none_or(|old| old == &bytes), "worker reply changed");
                journal.commit_source(run, &source, old.as_deref(), &bytes, vec![])?;
            }
            command::Body::ProposalDecision(decision) => {
                let (_, state) = state(&journal, run)?;
                ensure!(
                    state.proposals.iter().any(|p| p.id == decision.proposal_id),
                    "unknown proposal"
                );
                let value = json!({"code": decision.code, "reason": decision.reason,
                    "result": serde_json::from_slice::<Value>(&decision.result_json).unwrap_or(Value::Null)});
                let key = decision_key(&decision.proposal_id);
                let bytes = serde_json::to_vec(&value)?;
                let old = journal.source(run, &key)?;
                ensure!(
                    old.as_ref().is_none_or(|old| old == &bytes),
                    "proposal decision changed"
                );
                journal.commit_source(run, &key, old.as_deref(), &bytes, vec![])?;
            }
            _ => anyhow::bail!("unsupported worker command"),
        }
        Ok(())
    }

    /// Accept loop. Each exchange runs in its own bounded task, so a client
    /// that connects and stalls, or a read waiting on an absent hub, cannot hold
    /// up another worker's CLI. Accept and clock faults back off and retry.
    pub async fn serve(&self, listener: UnixListener, stamp: impl Fn() -> Result<v1::Stamp>) -> Result<()> {
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let mut failures = 0u32;
        loop {
            let permit = Arc::clone(&permits).acquire_owned().await?;
            let stream = match listener.accept().await {
                Ok((stream, _)) => {
                    failures = 0;
                    stream
                }
                Err(error) => {
                    tracing::warn!(%error, "worker accept failed");
                    let delay = if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) {
                        50
                    } else {
                        (10u64 << failures.min(6)).min(500)
                    };
                    failures = failures.saturating_add(1);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }
            };
            let stamp = match stamp() {
                Ok(stamp) => stamp,
                Err(error) => {
                    tracing::warn!(%error, "worker RPC dropped: no clock stamp");
                    continue;
                }
            };
            let gateway = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                // Bound the entire exchange, including idle clients and hub waits.
                match tokio::time::timeout(Duration::from_secs(5), gateway.exchange(stream, stamp)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "worker RPC rejected"),
                    Err(error) => tracing::warn!(%error, "worker RPC deadline"),
                }
            });
        }
    }

    async fn exchange(&self, stream: UnixStream, stamp: v1::Stamp) -> Result<()> {
        let credentials = stream.peer_cred()?;
        let records = lock(&self.journal)?
            .list_executions()?
            .into_iter()
            .map(|bytes| serde_json::from_slice(&bytes))
            .collect::<std::result::Result<Vec<ExecutionRecord>, _>>()?;
        let attributed = authenticate(
            &records,
            credentials.pid().context("missing peer pid")?,
            credentials.uid(),
            &stamp.boot_id,
        );
        let mut stream = BufReader::new(stream);
        let mut line = Vec::new();
        (&mut stream).take(MAX_REQUEST + 1).read_until(b'\n', &mut line).await?;
        ensure!(
            line.len() as u64 <= MAX_REQUEST && line.ends_with(b"\n"),
            "worker request exceeds limit"
        );
        let envelope: FrontendRequestEnvelope = serde_json::from_slice(&line)?;
        let event = match attributed {
            Ok(execution) => match self.handle(&execution, envelope.payload, &stamp).await {
                Ok(event) => event,
                Err(error) => FrontendEvent::WorkError {
                    message: format!("{error:#}"),
                },
            },
            Err(error) => FrontendEvent::WorkError {
                message: format!("worker_attribution_denied: {error}"),
            },
        };
        let mut bytes = serde_json::to_vec(&FrontendEventEnvelope::response(envelope.request_id, event))?;
        bytes.push(b'\n');
        stream.get_mut().write_all(&bytes).await?;
        Ok(())
    }

    async fn handle(
        &self,
        execution: &ExecutionRecord,
        request: FrontendRequest,
        stamp: &v1::Stamp,
    ) -> Result<FrontendEvent> {
        let claimed = match &request {
            FrontendRequest::GetWorkerContext { run_id }
            | FrontendRequest::GetPrBody { run_id }
            | FrontendRequest::GetPrStatus { run_id, .. }
            | FrontendRequest::SubmitProposal { run_id, .. }
            | FrontendRequest::ListProposals { run_id, .. }
            | FrontendRequest::SubmitAttachment { run_id, .. }
            | FrontendRequest::CommentsRecordGuideOutcome { run_id, .. }
            | FrontendRequest::ListAttachments { run_id } => run_id,
            _ => anyhow::bail!("worker_method_denied: method is outside worker-api/1"),
        };
        // BOSS_RUN_ID is historically the execution id in worker CLI calls.
        ensure!(
            claimed == &execution.execution_id,
            "worker_attribution_mismatch: request belongs to another run"
        );
        let run = identity(execution);
        match request {
            FrontendRequest::SubmitProposal {
                kind,
                payload,
                idempotency_key,
                ..
            } => {
                if kind == boss_protocol::ProposalKind::RunDone
                    && let Err(error) = self.capture_outputs(execution, stamp)
                {
                    // Lifecycle records outrank bulk data: done is queued regardless.
                    tracing::warn!(run = execution.run_id, %error, "worker output capture failed at done");
                }
                self.submit(execution, kind, payload, idempotency_key, stamp).await
            }
            FrontendRequest::CommentsRecordGuideOutcome {
                comment_id,
                disposition,
                body,
                request_regeneration,
                ..
            } => self.submit_guide_outcome(execution, comment_id, disposition, body, request_regeneration, stamp),
            FrontendRequest::ListProposals {
                kind, state: filter, ..
            } => {
                ensure!(
                    filter.is_none(),
                    "worker_unsupported: proposal-state filtering needs the hub's authoritative state and is not \
                     available on a spoke; omit --state to list this run's local receipts"
                );
                let journal = lock(&self.journal)?;
                let (_, state) = state(&journal, &run)?;
                let mut receipts: Vec<_> = state
                    .proposals
                    .into_iter()
                    .filter(|r| kind.is_none_or(|k| k.as_str() == r.kind))
                    .collect();
                for receipt in &mut receipts {
                    receipt.decision = journal
                        .source(&run, &decision_key(&receipt.id))?
                        .map(|b| serde_json::from_slice(&b))
                        .transpose()?;
                }
                Ok(response("local_receipts", json!({"proposals": receipts})))
            }
            FrontendRequest::SubmitAttachment { path, caption, .. } => self.attach(execution, &path, caption, stamp),
            FrontendRequest::ListAttachments { .. } => {
                let (_, state) = state(&*lock(&self.journal)?, &run)?;
                Ok(response("local_receipts", json!({"artifacts": state.artifacts})))
            }
            FrontendRequest::GetWorkerContext { .. } => {
                self.read(&run, v1::WorkerMethod::RunContext, false, stamp).await
            }
            FrontendRequest::GetPrBody { .. } => self.read(&run, v1::WorkerMethod::PrBody, false, stamp).await,
            FrontendRequest::GetPrStatus { refresh, .. } => {
                self.read(&run, v1::WorkerMethod::PrStatus, refresh, stamp).await
            }
            _ => unreachable!(),
        }
    }

    async fn read(
        &self,
        run: &v1::Run,
        method: v1::WorkerMethod,
        refresh: bool,
        stamp: &v1::Stamp,
    ) -> Result<FrontendEvent> {
        if !refresh {
            let cached = lock(&self.journal)?.source(run, &format!("worker-cache:{}", method as i32))?;
            if let Some(bytes) = cached {
                let cached: Cached = serde_json::from_slice(&bytes)?;
                return Ok(response("cached", serde_json::to_value(cached)?));
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        let key = format!("worker-request:{id}");
        let reply_key = format!("worker-reply:{id}");
        lock(&self.journal)?.commit_source(
            run,
            &key,
            None,
            &serde_json::to_vec(&(method as i32))?,
            vec![(
                v1::Record {
                    run: Some(run.clone()),
                    observed: Some(stamp.clone()),
                    body: Some(record::Body::Request(v1::WorkerRequest {
                        request_id: id.clone(),
                        method: method as i32,
                        proposal_id: String::new(),
                        refresh,
                    })),
                    ..Default::default()
                },
                Admission::Lifecycle,
            )],
        )?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let reply = lock(&self.journal)?.source(run, &reply_key)?;
            if let Some(bytes) = reply {
                // Consumed: the cache holds anything worth keeping.
                lock(&self.journal)?.delete_sources(run, &[key, reply_key])?;
                let value: Value = serde_json::from_slice(&bytes)?;
                ensure!(
                    value["code"].as_i64() == Some(v1::ResultCode::Ok as i64),
                    "hub_rejected: {}",
                    value["reason"]
                );
                return Ok(response("hub", value));
            }
            if tokio::time::Instant::now() >= deadline {
                // The wait is over; a reply that arrives later finds no request.
                lock(&self.journal)?.delete_sources(run, &[key])?;
                anyhow::bail!("hub_unavailable: no current hub reply; request {id}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

use boss_engine_proposal_validation::{self as validation, ProposalCounts};
use boss_protocol::{ProposalErrorCode, ProposalFieldError, ProposalKind, ProposalSubmissionError};

#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    pub id: String,
    /// `ProposalKind::as_str`, or the label of a run-scoped mutation.
    pub kind: String,
    pub digest: String,
    pub decision: Option<Value>,
}

/// A validated submission, ready to become a durable record.
struct Queued {
    id: String,
    label: String,
    wire_kind: v1::ProposalKind,
    contract: &'static str,
    canonical: Vec<u8>,
}

impl Gateway {
    async fn submit(
        &self,
        execution: &ExecutionRecord,
        kind: ProposalKind,
        payload: Value,
        key: Option<String>,
        stamp: &v1::Stamp,
    ) -> Result<FrontendEvent> {
        let validated = match validation::validate_payload(kind, &payload) {
            Ok(validated) => validated,
            Err(fields) => {
                return Ok(FrontendEvent::ProposalRejected {
                    error: ProposalSubmissionError::validation(fields),
                });
            }
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = match validation::select_idempotency_key(
            &execution.execution_id,
            kind,
            &validated.canonical_json,
            key.as_deref(),
            nanos,
        ) {
            Ok(id) => id,
            Err(field) => {
                return Ok(FrontendEvent::ProposalRejected {
                    error: ProposalSubmissionError::validation(vec![field]),
                });
            }
        };
        // Preserve the exact validated frontend payload, including its kind;
        // the hub reuses the same validator and role authorization on replay.
        let canonical = serde_json::to_vec(
            &json!({"kind": kind, "payload": serde_json::from_str::<Value>(&validated.canonical_json)?, "idempotency_key": id}),
        )?;
        // What the transcript source is right now, never the ingestion cursor:
        // a lagging or credit-starved tail must not understate completion
        // evidence. Resolved before the journal lock is taken.
        let watermark = (self.probe)(execution.clone()).await;
        let queued = Queued {
            id,
            label: kind.as_str().to_owned(),
            wire_kind: wire_kind(kind),
            contract: "proposal-json/1",
            canonical,
        };
        self.enqueue(
            execution,
            queued,
            |state| {
                validation::check_rate_caps(
                    kind,
                    ProposalCounts {
                        total: state.proposals.len(),
                        for_kind: state.proposals.iter().filter(|p| p.kind == kind.as_str()).count(),
                    },
                )
            },
            Some(watermark),
            stamp,
        )
    }

    /// `boss comment guide-outcome`: a run-scoped mutation. It is queued as a
    /// validated record bound to the claimed run; the hub performs the same
    /// revision-ownership checks the local engine does when it applies it.
    fn submit_guide_outcome(
        &self,
        execution: &ExecutionRecord,
        comment_id: String,
        disposition: boss_protocol::GuideCommentDisposition,
        body: String,
        request_regeneration: bool,
        stamp: &v1::Stamp,
    ) -> Result<FrontendEvent> {
        let mut fields = Vec::new();
        if body.trim().is_empty() {
            fields.push(ProposalFieldError::new("body", "guide outcome body may not be empty"));
        }
        if body.len() > MAX_GUIDE_BODY_BYTES {
            fields.push(ProposalFieldError::new(
                "body",
                format!("guide outcome body is over the {MAX_GUIDE_BODY_BYTES}-byte limit"),
            ));
        }
        if comment_id.trim().is_empty() || comment_id.len() > 256 {
            fields.push(ProposalFieldError::new(
                "comment_id",
                "comment id must be non-empty and at most 256 bytes",
            ));
        }
        if !fields.is_empty() {
            return Ok(FrontendEvent::ProposalRejected {
                error: ProposalSubmissionError::validation(fields),
            });
        }
        let canonical = serde_json::to_vec(&json!({
            "run_id": execution.execution_id, "comment_id": comment_id, "disposition": disposition,
            "body": body, "request_regeneration": request_regeneration,
        }))?;
        let digest = boss_engine_attachments::store::digest_hex(&canonical);
        // Re-recording the same comment with new content is a new record that
        // supersedes the old one; an identical retry replays.
        let queued = Queued {
            id: format!("guide-outcome:{comment_id}:{}", &digest[..16]),
            label: GUIDE_OUTCOME_KIND.into(),
            wire_kind: v1::ProposalKind::OtherValidated,
            contract: GUIDE_OUTCOME_CONTRACT,
            canonical,
        };
        self.enqueue(
            execution,
            queued,
            |state| {
                let for_kind = state.proposals.iter().filter(|p| p.kind == GUIDE_OUTCOME_KIND).count();
                if state.proposals.len() >= boss_protocol::PROPOSAL_CAP_TOTAL_PER_EXECUTION
                    || for_kind >= boss_protocol::PROPOSAL_CAP_PER_KIND_PER_EXECUTION
                {
                    return Err(ProposalSubmissionError::new(
                        ProposalErrorCode::RateLimited,
                        "this execution has reached its guide outcome submission cap",
                    ));
                }
                Ok(())
            },
            None,
            stamp,
        )
    }

    /// Commit one queued record and its receipt atomically. `watermark` is the
    /// transcript probe result; a failed probe becomes an explicit gap record
    /// ahead of the proposal, never a silently empty watermark.
    fn enqueue(
        &self,
        execution: &ExecutionRecord,
        queued: Queued,
        caps: impl Fn(&State) -> std::result::Result<(), ProposalSubmissionError>,
        watermark: Option<Result<(String, u64)>>,
        stamp: &v1::Stamp,
    ) -> Result<FrontendEvent> {
        // The canonical bytes carry the kind, payload and key, so equal digests
        // mean an identical submission.
        let digest = boss_engine_attachments::store::digest_hex(&queued.canonical);
        let run = identity(execution);
        let mut journal = lock(&self.journal)?;
        let (old, mut state) = state(&journal, &run)?;
        if let Some(receipt) = state.proposals.iter().find(|p| p.id == queued.id) {
            ensure!(
                receipt.digest == digest && receipt.kind == queued.label,
                "idempotency key reused with different proposal"
            );
            let mut receipt = receipt.clone();
            receipt.decision = journal
                .source(&run, &decision_key(&receipt.id))?
                .map(|b| serde_json::from_slice(&b))
                .transpose()?;
            return Ok(receipt_event(&receipt, true));
        }
        if let Err(error) = caps(&state) {
            return Ok(FrontendEvent::ProposalRejected { error });
        }
        let mut observations = Vec::new();
        let (transcript_generation, transcript_through) = match watermark {
            Some(Ok(observed)) => observed,
            Some(Err(error)) => {
                observations.push((
                    v1::Record {
                        run: Some(run.clone()),
                        observed: Some(stamp.clone()),
                        body: Some(record::Body::Gap(v1::Gap {
                            resource: "transcript".into(),
                            reason: format!("transcript unavailable at proposal: {error:#}"),
                            ..Default::default()
                        })),
                        ..Default::default()
                    },
                    Admission::Lifecycle,
                ));
                (String::new(), 0)
            }
            None => (String::new(), 0),
        };
        let proposal = v1::Proposal {
            proposal_id: queued.id.clone(),
            kind: queued.wire_kind as i32,
            payload_contract: queued.contract.into(),
            canonical_json: queued.canonical,
            transcript_generation,
            transcript_through,
        };
        proposal.validate()?;
        state.proposals.push(Receipt {
            id: queued.id,
            kind: queued.label,
            digest,
            decision: None,
        });
        observations.push((
            v1::Record {
                run: Some(run.clone()),
                observed: Some(stamp.clone()),
                body: Some(record::Body::Proposal(proposal)),
                ..Default::default()
            },
            Admission::Lifecycle,
        ));
        journal.commit_source(&run, STATE, old.as_deref(), &serde_json::to_vec(&state)?, observations)?;
        Ok(receipt_event(state.proposals.last().context("missing receipt")?, false))
    }
}

fn decision_key(id: &str) -> String {
    format!(
        "worker-decision:{}",
        boss_engine_attachments::store::digest_hex(id.as_bytes())
    )
}

fn receipt_event(receipt: &Receipt, duplicate: bool) -> FrontendEvent {
    response(
        if receipt.decision.is_some() {
            "hub_decision"
        } else {
            "queued"
        },
        json!({"receipt": receipt, "already_submitted": duplicate,
            "message": "Durably queued for hub validation. A local receipt does not finalize the run or release its workspace."}),
    )
}

fn wire_kind(kind: ProposalKind) -> v1::ProposalKind {
    match kind {
        ProposalKind::PrCreated => v1::ProposalKind::PrCreated,
        ProposalKind::RunDone => v1::ProposalKind::RunDone,
        ProposalKind::Wait => v1::ProposalKind::Wait,
        ProposalKind::ReviewReport => v1::ProposalKind::ReviewReport,
        ProposalKind::ReviewVerdict => v1::ProposalKind::ReviewVerdict,
        _ => v1::ProposalKind::OtherValidated,
    }
}

#[cfg(test)]
mod tests {
    use crate::{Gateway, identity};
    use boss_host_execution::{
        process::process_snapshot,
        worker::{ExecutionRecord, ObservationConfig, Phase},
    };
    use boss_protocol::{
        FrontendEvent, PrBodyView, Product, Task, TaskKind, TaskStatus, WorkItemDependencyDetail, WorkerContextBundle,
    };
    use boss_protocol_spoke::v1::{self, command, record};
    use boss_spoke_journal::{Journal, Limits};
    use prost::Message;
    use serde_json::Value;
    use std::{
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::net::UnixListener;

    fn stamp() -> anyhow::Result<v1::Stamp> {
        Ok(v1::Stamp {
            boot_id: "boot".into(),
            wall_unix_ms: 1234,
            awake_ns: 1,
            elapsed_ns: 1,
        })
    }

    /// Reports the size of `<root>/transcript.log` as the observed source, or
    /// that no transcript exists, independent of anything ingested.
    fn probe(root: &Path) -> crate::TranscriptProbe {
        let path = root.join("transcript.log");
        Arc::new(move |_| {
            let path = path.clone();
            Box::pin(async move { Ok(("1-0".to_owned(), std::fs::metadata(path)?.len())) })
        })
    }

    fn open_with(root: &Path, limits: Limits) -> Gateway {
        Gateway {
            journal: Arc::new(Mutex::new(Journal::open(&root.join("state"), "host", limits).unwrap())),
            state_dir: root.join("state"),
            probe: probe(root),
        }
    }

    fn open(root: &Path) -> Gateway {
        open_with(root, Limits::default())
    }

    fn execution(root: &Path) -> ExecutionRecord {
        ExecutionRecord::builder()
            .execution_id("execution")
            .run_id("run")
            .spawn_token("token")
            .assignment_generation(1)
            .lease_id("lease")
            .workspace_id("workspace")
            .session("session")
            .phase(Phase::Alive)
            .boot_id("boot")
            .pid(std::process::id() as u64)
            .driver_id("claude")
            .observation(ObservationConfig {
                workspace: root.into(),
                pending: root.into(),
                file: None,
                pid_start: Some(process_snapshot(std::process::id() as i32).unwrap().start),
            })
            .build()
    }

    fn save(gateway: &Gateway, execution: &ExecutionRecord) {
        gateway
            .journal
            .lock()
            .unwrap()
            .save_execution(
                &execution.run_id,
                &execution.spawn_token,
                &serde_json::to_vec(execution).unwrap(),
            )
            .unwrap();
    }

    fn records(gateway: &Gateway) -> Vec<v1::Record> {
        let journal = gateway.journal.lock().unwrap();
        let mut result = Vec::new();
        let mut after = 0;
        while let Some(record) = journal.next_record(after).unwrap() {
            after = record.sequence;
            result.push(v1::Record::decode(record.bytes.as_slice()).unwrap());
        }
        result
    }

    async fn cli(socket: &Path, run: &str, args: &[&str]) -> std::process::Output {
        let bin = std::env::current_dir()
            .unwrap()
            .join(std::env::var("BOSS_TEST_CLI").unwrap());
        tokio::process::Command::new(bin)
            .env("BOSS_SOCKET_PATH", socket)
            .env("BOSS_RUN_ID", run)
            .args(args)
            .arg("--json")
            .output()
            .await
            .unwrap()
    }

    fn success(output: std::process::Output) -> Value {
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn server(gateway: &Gateway, socket: &Path) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        let listener = UnixListener::bind(socket).unwrap();
        let gateway = gateway.clone();
        tokio::spawn(async move { gateway.serve(listener, stamp).await })
    }

    fn command(run: &v1::Run, body: command::Body) -> v1::Command {
        v1::Command {
            sequence: 1,
            command_id: "command".into(),
            run: Some(run.clone()),
            not_after_unix_ms: 99999,
            body: Some(body),
        }
    }

    #[tokio::test]
    async fn actual_cli_queues_done_preserves_assignment_and_replays_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        let output = root.path().join("pr-url.json");
        std::fs::write(&output, br#"{"pr_url":"https://github.com/spinyfin/mono/pull/123"}"#).unwrap();
        execution
            .worker_outputs
            .insert("BOSS_PR_URL_OUTPUT".into(), output.display().to_string());
        let gateway = open(root.path());
        save(&gateway, &execution);
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let args = [
            "propose",
            "done",
            "--outcome",
            "delivered",
            "--summary",
            "Implemented the gateway",
        ];
        let first = success(cli(&socket, "execution", &args).await);
        assert_eq!(first["status"], "queued");
        assert_eq!(first["value"]["already_submitted"], false);
        let id = first["value"]["receipt"]["id"].as_str().unwrap().to_owned();
        let second = success(cli(&socket, "execution", &args).await);
        assert_eq!(second["value"]["already_submitted"], true);
        assert_eq!(
            records(&gateway)
                .iter()
                .filter(|r| matches!(r.body, Some(record::Body::Proposal(_))))
                .count(),
            1
        );
        let stored: ExecutionRecord = serde_json::from_slice(
            &gateway
                .journal
                .lock()
                .unwrap()
                .load_execution("run", "token")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stored.phase, Phase::Alive);
        assert!(!stored.reclaimed);
        assert_eq!(stored.lease_id, "lease");
        task.abort();
        let _ = task.await;
        drop(gateway);
        std::fs::remove_file(&socket).unwrap();
        std::fs::remove_file(&output).unwrap();
        let gateway = open(root.path());
        gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
        assert!(records(&gateway).iter().any(|r| matches!(&r.body, Some(record::Body::Artifact(a)) if a.kind == v1::ArtifactKind::PrUrl as i32 && a.eof && String::from_utf8_lossy(&a.data).contains("/pull/123"))));
        let task = server(&gateway, &socket);
        let replay = success(cli(&socket, "execution", &args).await);
        assert_eq!(replay["value"]["receipt"]["id"], id);
        assert_eq!(replay["value"]["already_submitted"], true);
        let decision = command(
            &identity(&execution),
            command::Body::ProposalDecision(v1::ProposalDecision {
                proposal_id: id,
                code: v1::ResultCode::Rejected as i32,
                reason: "validation evidence missing".into(),
                result_json: b"{}".to_vec(),
            }),
        );
        gateway.apply(&decision).unwrap();
        gateway.apply(&decision).unwrap();
        let rejected = cli(&socket, "execution", &args).await;
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("validation evidence missing"));
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn peer_attribution_validators_and_default_deny_survive_real_cli_calls() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let gateway = open(root.path());
        save(&gateway, &execution);
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let denied = cli(
            &socket,
            "sibling",
            &["propose", "done", "--outcome", "delivered", "--summary", "wrong run"],
        )
        .await;
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("worker_attribution_mismatch"));
        let denied = cli(&socket, "execution", &["pr", "status"]).await;
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("hub_unavailable"));
        let invalid = cli(
            &socket,
            "execution",
            &["propose", "pr-created", "--url", "https://example.com/invalid"],
        )
        .await;
        assert!(!invalid.status.success());
        assert!(
            !records(&gateway)
                .iter()
                .any(|r| matches!(r.body, Some(record::Body::Proposal(_))))
        );
        let denied = cli(&socket, "execution", &["product", "list"]).await;
        assert!(!denied.status.success());
        // Direct protocol client exercises the default deny even if CLI spelling evolves.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let mut bytes = serde_json::to_vec(&boss_protocol::FrontendRequestEnvelope {
            request_id: "deny".into(),
            payload: boss_protocol::FrontendRequest::ListProducts,
        })
        .unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert!(reply.contains("worker_method_denied"));
        // A matching environment variable cannot replace ancestry and incarnation.
        let mut recycled = execution.clone();
        recycled.observation.as_mut().unwrap().pid_start = Some(0);
        save(&gateway, &recycled);
        let denied = cli(&socket, "execution", &["pr", "body"]).await;
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("worker_attribution_denied"));
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn context_snapshot_and_hub_reply_keep_revision_and_observation_time() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let gateway = open(root.path());
        save(&gateway, &execution);
        let bundle = WorkerContextBundle {
            task: Task::builder()
                .id("task")
                .product_id("product")
                .created_at("1")
                .description("Build gateway")
                .kind(TaskKind::Task)
                .name("Gateway")
                .status(TaskStatus::Todo)
                .updated_at("1")
                .build(),
            product: Product::builder()
                .id("product")
                .created_at("1")
                .description("Workers")
                .name("Boss")
                .slug("boss")
                .status("active")
                .updated_at("1")
                .build(),
            project: None,
            sibling_tasks: vec![],
            attention_groups: vec![],
            proposals: vec![],
            own_dependencies: WorkItemDependencyDetail {
                work_item_id: "task".into(),
                dependents: vec![],
                prerequisites: vec![],
            },
        };
        gateway
            .provision(
                &command(
                    &identity(&execution),
                    command::Body::Spawn(v1::Spawn {
                        run_context_json: serde_json::to_vec(&bundle).unwrap(),
                        context_revision: "snapshot-7".into(),
                        ..Default::default()
                    }),
                ),
                &stamp().unwrap(),
            )
            .unwrap();
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let cached = cli(&socket, "execution", &["context"]).await;
        let freshness = String::from_utf8_lossy(&cached.stderr);
        assert!(freshness.contains("cached") && freshness.contains("snapshot-7") && freshness.contains("1234"));
        assert_eq!(success(cached)["task"]["name"], "Gateway");
        let fetch = tokio::spawn({
            let socket = socket.clone();
            async move { cli(&socket, "execution", &["pr", "body"]).await }
        });
        let request = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(request) = records(&gateway).into_iter().find_map(|r| match r.body {
                    Some(record::Body::Request(r)) => Some(r),
                    _ => None,
                }) {
                    break request;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let reply = command(
            &identity(&execution),
            command::Body::WorkerReply(v1::WorkerReply {
                request_id: request.request_id,
                code: v1::ResultCode::Ok as i32,
                result_json: serde_json::to_vec(&FrontendEvent::PrBodyResult {
                    body: PrBodyView {
                        pr_url: None,
                        title: Some("Gateway".into()),
                        body: Some("Evidence".into()),
                    },
                })
                .unwrap(),
                revision: "hub-8".into(),
                observed_unix_ms: 4567,
                reason: String::new(),
            }),
        );
        let mut foreign = reply.clone();
        foreign.run.as_mut().unwrap().spawn_token = "foreign".into();
        assert!(gateway.apply(&foreign).is_err());
        foreign = reply.clone();
        foreign.run.as_mut().unwrap().assignment_generation += 1;
        assert!(gateway.apply(&foreign).is_err());
        gateway.apply(&reply).unwrap();
        let live = fetch.await.unwrap();
        assert!(String::from_utf8_lossy(&live.stderr).contains("hub-8"));
        assert_eq!(success(live)["body"], "Evidence");
        let cached = cli(&socket, "execution", &["pr", "body"]).await;
        let freshness = String::from_utf8_lossy(&cached.stderr);
        assert!(freshness.contains("cached") && freshness.contains("4567"));
        assert_eq!(success(cached)["title"], "Gateway");
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn artifacts_are_validated_durable_and_chunked_without_losing_metadata() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let gateway = open(root.path());
        save(&gateway, &execution);
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let path = root.path().join("image.png");
        // The existing image validator reads the PNG signature and IHDR dimensions.
        let mut bytes = vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0,
        ];
        bytes.resize(150_000, 0);
        std::fs::write(&path, &bytes).unwrap();
        let args = ["attach", path.to_str().unwrap(), "--caption", "gateway evidence"];
        let attached = success(cli(&socket, "execution", &args).await);
        assert_eq!(attached["status"], "queued");
        assert_eq!(
            success(cli(&socket, "execution", &args).await)["value"]["already_registered"],
            true
        );
        assert!(
            records(&gateway).is_empty(),
            "registration does not preallocate bulk sequences"
        );
        std::fs::remove_file(&path).unwrap();
        for _ in 0..3 {
            gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
        }
        let chunks: Vec<_> = records(&gateway)
            .into_iter()
            .filter_map(|r| match r.body {
                Some(record::Body::Artifact(a)) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(chunks.len(), 3);
        assert!(chunks.last().unwrap().eof);
        assert_eq!(chunks.iter().flat_map(|c| c.data.clone()).collect::<Vec<_>>(), bytes);
        assert_eq!(
            serde_json::from_slice::<Value>(&chunks[0].metadata_json).unwrap()["caption"],
            "gateway evidence"
        );
        for suffix in 1..boss_protocol::ATTACHMENT_CAP_PER_EXECUTION {
            bytes[100] = suffix as u8;
            std::fs::write(&path, &bytes).unwrap();
            success(cli(&socket, "execution", &args).await);
        }
        bytes[100] = 255;
        std::fs::write(&path, &bytes).unwrap();
        assert!(!cli(&socket, "execution", &args).await.status.success());
        // Filling the attachment allowance must not prevent capturing a PR.
        let output = root.path().join("pr-url.json");
        std::fs::write(&output, br#"{"pr_url":"https://github.com/spinyfin/mono/pull/123"}"#).unwrap();
        let mut execution = execution.clone();
        execution
            .worker_outputs
            .insert("BOSS_PR_URL_OUTPUT".into(), output.display().to_string());
        gateway.capture_outputs(&execution, &stamp().unwrap()).unwrap();
        let malformed = root.path().join("invalid.png");
        std::fs::write(&malformed, b"not an image").unwrap();
        assert!(
            !cli(&socket, "execution", &["attach", malformed.to_str().unwrap()])
                .await
                .status
                .success()
        );
        assert!(
            !cli(
                &socket,
                "execution",
                &["attach", malformed.to_str().unwrap(), "--caption", &"x".repeat(241)]
            )
            .await
            .status
            .success()
        );
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn failed_durable_commit_never_returns_a_queued_receipt() {
        let root = tempfile::tempdir().unwrap();
        let gateway = open_with(
            root.path(),
            Limits {
                spool_bytes: 4096,
                lifecycle_reserve: 1024,
            },
        );
        let execution = execution(root.path());
        save(&gateway, &execution);
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let output = cli(
            &socket,
            "execution",
            &["propose", "wait", "--reason", &"r".repeat(4000), "--duration", "1m"],
        )
        .await;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("spool full"));
        assert!(records(&gateway).is_empty());
        assert!(
            gateway
                .journal
                .lock()
                .unwrap()
                .source(&identity(&execution), crate::STATE)
                .unwrap()
                .is_none()
        );
        task.abort();
        let _ = task.await;
    }

    fn proposals(gateway: &Gateway) -> Vec<v1::Proposal> {
        records(gateway)
            .into_iter()
            .filter_map(|r| match r.body {
                Some(record::Body::Proposal(p)) => Some(p),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn keyless_wait_renews_while_keyed_and_other_proposals_replay() {
        let root = tempfile::tempdir().unwrap();
        let gateway = open(root.path());
        save(&gateway, &execution(root.path()));
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let wait = ["propose", "wait", "--reason", "CI is running", "--duration", "1m"];
        let first = success(cli(&socket, "execution", &wait).await);
        let second = success(cli(&socket, "execution", &wait).await);
        assert_eq!(first["value"]["already_submitted"], false);
        assert_eq!(second["value"]["already_submitted"], false);
        assert_ne!(first["value"]["receipt"]["id"], second["value"]["receipt"]["id"]);
        assert_eq!(proposals(&gateway).len(), 2);
        let keyed = [
            "propose",
            "wait",
            "--reason",
            "CI is running",
            "--duration",
            "1m",
            "--idempotency-key",
            "wait-1",
        ];
        success(cli(&socket, "execution", &keyed).await);
        let again = success(cli(&socket, "execution", &keyed).await);
        assert_eq!(again["value"]["already_submitted"], true);
        assert_eq!(proposals(&gateway).len(), 3);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn guide_outcome_is_a_queued_run_scoped_mutation() {
        let root = tempfile::tempdir().unwrap();
        let gateway = open(root.path());
        save(&gateway, &execution(root.path()));
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let args = [
            "comment",
            "guide-outcome",
            "--comment-id",
            "cmt_1",
            "--disposition",
            "answered",
            "--body",
            "The guide already covers this",
        ];
        let queued = success(cli(&socket, "execution", &args).await);
        assert_eq!(queued["status"], "queued");
        assert_eq!(queued["value"]["receipt"]["kind"], "guide_outcome");
        let repeat = success(cli(&socket, "execution", &args).await);
        assert_eq!(repeat["value"]["already_submitted"], true);
        let stored = proposals(&gateway);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].payload_contract, "guide-outcome/1");
        let payload: Value = serde_json::from_slice(&stored[0].canonical_json).unwrap();
        assert_eq!(payload["run_id"], "execution");
        assert_eq!(payload["comment_id"], "cmt_1");
        assert_eq!(payload["disposition"], "answered");
        // Bound to the caller's own run, and body is required.
        let foreign = cli(&socket, "sibling", &args).await;
        assert!(!foreign.status.success());
        assert!(String::from_utf8_lossy(&foreign.stderr).contains("worker_attribution_mismatch"));
        let empty = cli(
            &socket,
            "execution",
            &[
                "comment",
                "guide-outcome",
                "--comment-id",
                "cmt_1",
                "--disposition",
                "no_change",
                "--body",
                "   ",
            ],
        )
        .await;
        assert!(!empty.status.success());
        assert_eq!(proposals(&gateway).len(), 1);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn done_watermark_is_the_observed_source_not_the_ingested_cursor() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let gateway = open(root.path());
        save(&gateway, &execution);
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let done = ["propose", "done", "--outcome", "delivered", "--summary", "Done"];
        // The ingestion cursor stands at 100 bytes while the file has 5000.
        gateway
            .journal
            .lock()
            .unwrap()
            .commit_source(
                &identity(&execution),
                "transcript",
                None,
                br#"{"incarnation":1,"cursor":{"generation":0,"offset":100}}"#,
                vec![],
            )
            .unwrap();
        std::fs::write(root.path().join("transcript.log"), vec![b'x'; 5000]).unwrap();
        success(cli(&socket, "execution", &done).await);
        let stored = proposals(&gateway);
        assert_eq!(stored[0].transcript_through, 5000);
        assert_eq!(stored[0].transcript_generation, "1-0");
        // An unresolvable source is explicit evidence, never a silent zero.
        std::fs::remove_file(root.path().join("transcript.log")).unwrap();
        success(
            cli(
                &socket,
                "execution",
                &["propose", "wait", "--reason", "x", "--duration", "1m"],
            )
            .await,
        );
        assert!(records(&gateway).iter().any(|r| matches!(
            &r.body,
            Some(record::Body::Gap(g)) if g.resource == "transcript" && g.reason.contains("unavailable")
        )));
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn done_is_queued_even_when_output_capture_fails() {
        let root = tempfile::tempdir().unwrap();
        let mut first = execution(root.path());
        let structured = root.path().join("structured.json");
        std::fs::write(&structured, b"this is not json").unwrap();
        first
            .worker_outputs
            .insert("BOSS_STRUCTURED_OUTPUT".into(), structured.display().to_string());
        let gateway = open(root.path());
        save(&gateway, &first);
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let done = ["propose", "done", "--outcome", "delivered", "--summary", "Done"];
        let queued = success(cli(&socket, "execution", &done).await);
        assert_eq!(queued["status"], "queued");
        assert!(records(&gateway).iter().any(|r| matches!(
            &r.body,
            Some(record::Body::Gap(g)) if g.resource == "output:BOSS_STRUCTURED_OUTPUT" && g.reason.contains("not JSON")
        )));
        task.abort();
        let _ = task.await;

        // A nearly full bulk allowance cannot stop done either.
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        let output = root.path().join("pr-url.json");
        std::fs::write(&output, br#"{"pr_url":"https://github.com/spinyfin/mono/pull/123"}"#).unwrap();
        execution
            .worker_outputs
            .insert("BOSS_PR_URL_OUTPUT".into(), output.display().to_string());
        let gateway = open_with(
            root.path(),
            Limits {
                spool_bytes: 6000,
                lifecycle_reserve: 2500,
            },
        );
        save(&gateway, &execution);
        gateway
            .journal
            .lock()
            .unwrap()
            .commit_source(
                &identity(&execution),
                "worker-blob:filler:0",
                None,
                &[7u8; 3000],
                vec![],
            )
            .unwrap();
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        let queued = success(cli(&socket, "execution", &done).await);
        assert_eq!(queued["status"], "queued");
        assert_eq!(proposals(&gateway).len(), 1);
        task.abort();
        let _ = task.await;
    }

    fn png(size: usize) -> Vec<u8> {
        let mut bytes = vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0,
        ];
        bytes.resize(size, 0);
        bytes
    }

    fn chunks(gateway: &Gateway) -> Vec<v1::ArtifactChunk> {
        records(gateway)
            .into_iter()
            .filter_map(|r| match r.body {
                Some(record::Body::Artifact(a)) => Some(a),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn staged_artifact_bytes_move_into_records_and_are_released_with_the_run() {
        let root = tempfile::tempdir().unwrap();
        let mut execution = execution(root.path());
        let gateway = open(root.path());
        save(&gateway, &execution);
        let baseline = gateway.journal.lock().unwrap().bytes().unwrap();
        let path = root.path().join("image.png");
        let bytes = png(150_000);
        std::fs::write(&path, &bytes).unwrap();
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        success(cli(&socket, "execution", &["attach", path.to_str().unwrap()]).await);
        task.abort();
        let _ = task.await;
        let staged = gateway.journal.lock().unwrap().bytes().unwrap();
        assert!(staged >= baseline + 150_000);
        // Not streamed yet, so a reclaim must keep the staged bytes.
        execution.reclaimed = true;
        gateway.release_if_done(&identity(&execution)).unwrap();
        assert!(
            gateway
                .journal
                .lock()
                .unwrap()
                .source(&identity(&execution), crate::STATE)
                .unwrap()
                .is_some()
        );
        gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
        let streamed = gateway.journal.lock().unwrap().bytes().unwrap();
        // Records carry the bytes now; staging no longer counts them a second time.
        assert!(
            streamed < staged + 16 * 1024,
            "streaming must not double the footprint: {staged} -> {streamed}"
        );
        assert_eq!(
            chunks(&gateway).iter().flat_map(|c| c.data.clone()).collect::<Vec<_>>(),
            bytes
        );
        gateway.release_if_done(&identity(&execution)).unwrap();
        let journal = gateway.journal.lock().unwrap();
        for source in [crate::STATE, "worker-blob:3"] {
            assert!(journal.source(&identity(&execution), source).unwrap().is_none());
        }
        let released = journal.bytes().unwrap();
        assert!(
            released < streamed,
            "gateway rows are released: {streamed} -> {released}"
        );
        assert!(released > baseline, "numbered records remain until receipted");
    }

    #[tokio::test]
    async fn an_accepted_artifact_reaches_eof_on_a_small_spool() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let gateway = open_with(
            root.path(),
            Limits {
                spool_bytes: 500_000,
                lifecycle_reserve: 50_000,
            },
        );
        save(&gateway, &execution);
        let path = root.path().join("image.png");
        let bytes = png(220_000);
        std::fs::write(&path, &bytes).unwrap();
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        success(cli(&socket, "execution", &["attach", path.to_str().unwrap()]).await);
        task.abort();
        let _ = task.await;
        for _ in 0..8 {
            gateway.pump(&identity(&execution), &stamp().unwrap()).unwrap();
        }
        let chunks = chunks(&gateway);
        assert!(chunks.last().unwrap().eof);
        assert_eq!(chunks.iter().flat_map(|c| c.data.clone()).collect::<Vec<_>>(), bytes);
    }

    #[tokio::test]
    async fn a_failed_registration_releases_what_it_staged() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let gateway = open_with(
            root.path(),
            Limits {
                spool_bytes: 200_000,
                lifecycle_reserve: 20_000,
            },
        );
        save(&gateway, &execution);
        let baseline = gateway.journal.lock().unwrap().bytes().unwrap();
        let path = root.path().join("image.png");
        std::fs::write(&path, png(400_000)).unwrap();
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        assert!(
            !cli(&socket, "execution", &["attach", path.to_str().unwrap()])
                .await
                .status
                .success()
        );
        task.abort();
        let _ = task.await;
        assert_eq!(gateway.journal.lock().unwrap().bytes().unwrap(), baseline);
    }

    #[tokio::test]
    async fn one_stalled_client_does_not_block_other_workers() {
        let root = tempfile::tempdir().unwrap();
        let gateway = open(root.path());
        save(&gateway, &execution(root.path()));
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        // Connects and says nothing for the whole exchange deadline.
        let _idle = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let started = std::time::Instant::now();
        success(
            cli(
                &socket,
                "execution",
                &["propose", "wait", "--reason", "x", "--duration", "1m"],
            )
            .await,
        );
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn consumed_and_expired_requests_leave_no_rows_behind() {
        let root = tempfile::tempdir().unwrap();
        let execution = execution(root.path());
        let gateway = open(root.path());
        save(&gateway, &execution);
        let socket = root.path().join("worker.sock");
        let task = server(&gateway, &socket);
        // No hub reply: the wait times out and its request row is deleted.
        let timed_out = cli(&socket, "execution", &["pr", "body"]).await;
        assert!(String::from_utf8_lossy(&timed_out.stderr).contains("hub_unavailable"));
        // Only the numbered request record remains, not a worker-request row.
        let request_id = records(&gateway)
            .into_iter()
            .find_map(|r| match r.body {
                Some(record::Body::Request(request)) => Some(request.request_id),
                _ => None,
            })
            .unwrap();
        assert!(
            gateway
                .journal
                .lock()
                .unwrap()
                .source(&identity(&execution), &format!("worker-request:{request_id}"))
                .unwrap()
                .is_none()
        );
        // A late reply for the closed request is ignored rather than refused.
        let late = command(
            &identity(&execution),
            command::Body::WorkerReply(v1::WorkerReply {
                request_id,
                code: v1::ResultCode::Ok as i32,
                result_json: b"{}".to_vec(),
                revision: "hub-9".into(),
                observed_unix_ms: 99,
                reason: String::new(),
            }),
        );
        gateway.apply(&late).unwrap();
        task.abort();
        let _ = task.await;
    }
}
