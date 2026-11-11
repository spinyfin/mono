//! Restricted worker IPC. Durable receipt never confers hub work authority.
//!
//! Command inventory: `execution-runner/src/runner/prompt.rs`,
//! `prompt_fragments.rs`, `review_prompt.rs`, and `progression_prompt.rs`
//! compose context, PR status/body, attachment, and proposal instructions.
//! `worker-api/1` accepts those bound reads, attachment registration/listing,
//! and the existing validated proposal kinds (including done, wait, review
//! reports/verdicts, blocked questions, followups and PR-created). Listing
//! returns this assignment's local receipts, explicitly labeled as such.
//! Context uses Spawn's sanitized bundle and context revision. Cache misses
//! and explicit refreshes emit WorkerRequest on the journal stream; absence
//! of a reply is hub_unavailable. Cached reads retain observation time.
//!
//! Prompt paths for progression supervision also mention arbitrary task reads
//! and stuck reports; CI/conflict recovery emits engine administration verbs.
//! These are denied, as are every general app/board/control method. Enabling
//! such execution kinds remotely requires their own run-scoped contracts.
//! Cube publication and gh remain host-local commands using host credentials.
//! The host-allocated PR/structured-output files are captured into durable
//! artifact sources and chunked over the same stream, including after restart.
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

    #[derive(Serialize, Deserialize)]
    pub(super) struct Artifact {
        id: String,
        kind: i32,
        size: usize,
        metadata: Value,
        offset: usize,
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
            // sequences ahead of lifecycle records. Partial staging is retryable.
            for (index, chunk) in bytes.chunks(CHUNK).enumerate() {
                journal.commit_source(&run, &format!("worker-blob:{id}:{index}"), None, chunk, vec![])?;
            }
            metadata["registered_unix_ms"] = stamp.wall_unix_ms.into();
            state.artifacts.push(Artifact {
                id,
                kind: kind as i32,
                size: bytes.len(),
                metadata,
                offset: 0,
            });
            journal.commit_source(&run, STATE, old.as_deref(), &serde_json::to_vec(&state)?, vec![])?;
            Ok(response(
                "queued",
                json!({"artifact": state.artifacts.last(), "already_registered": false}),
            ))
        }

        /// Capture only host-allocated output paths recorded before launching the
        /// worker. Worker-supplied paths and symlinks never become output authority.
        pub fn capture_outputs(&self, execution: &ExecutionRecord, stamp: &v1::Stamp) -> Result<()> {
            for (name, path) in &execution.worker_outputs {
                let kind = match name.as_str() {
                    "BOSS_PR_URL_OUTPUT" => v1::ArtifactKind::PrUrl,
                    "BOSS_STRUCTURED_OUTPUT" => v1::ArtifactKind::ReviewResult,
                    _ => continue,
                };
                let file = match std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)
                {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                ensure!(file.metadata()?.is_file(), "output is not a regular file");
                let mut bytes = Vec::new();
                file.take(boss_protocol::ATTACHMENT_MAX_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.is_empty() {
                    continue;
                }
                ensure!(
                    bytes.len() as u64 <= boss_protocol::ATTACHMENT_MAX_BYTES,
                    "output exceeds limit"
                );
                let value: Value = serde_json::from_slice(&bytes)?;
                if kind == v1::ArtifactKind::PrUrl {
                    boss_engine_proposal_validation::validate_payload(boss_protocol::ProposalKind::PrCreated, &value)
                        .map_err(|errors| anyhow::anyhow!("invalid PR output: {errors:?}"))?;
                }
                self.register(
                    execution,
                    kind,
                    &bytes,
                    json!({"output_kind": execution.worker_outputs.get("BOSS_STRUCTURED_OUTPUT_KIND"), "source": name}),
                    stamp,
                )?;
            }
            Ok(())
        }

        /// Number at most one credited chunk per run per tick, and retain a source
        /// cursor atomically with it. Restart never duplicates a chunk or loses EOF.
        pub fn pump(&self, run: &v1::Run, stamp: &v1::Stamp) -> Result<()> {
            let mut journal = lock(&self.journal)?;
            let (old, mut state) = state(&journal, run)?;
            let Some(artifact) = state.artifacts.iter_mut().find(|a| a.offset < a.size) else {
                return Ok(());
            };
            let credit = journal.bulk_credit()?;
            let chunk_index = artifact.offset / CHUNK;
            let source = journal
                .source(run, &format!("worker-blob:{}:{chunk_index}", artifact.id))?
                .context("artifact source missing")?;
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
            journal.commit_source(
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
            )
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
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

pub type SharedJournal = Arc<Mutex<Journal>>;
const MAX_REQUEST: u64 = 256 * 1024;
const STATE: &str = "worker-api";

#[derive(Clone)]
pub struct Gateway {
    pub journal: SharedJournal,
    pub state_dir: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    proposals: Vec<Receipt>,
    artifacts: Vec<artifacts::Artifact>,
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
    pub async fn observe(&self, stamp: impl Fn() -> Result<v1::Stamp>) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let rows = lock(&self.journal)?.list_executions()?;
            for bytes in rows {
                let execution: ExecutionRecord = serde_json::from_slice(&bytes)?;
                let stamp = stamp()?;
                if let Err(error) = self.capture_outputs(&execution, &stamp) {
                    tracing::warn!(run = execution.run_id, %error, "worker output retained for retry");
                }
                if let Err(error) = self.pump(&identity(&execution), &stamp)
                    && !boss_spoke_journal::Fault::is_full(&error)
                {
                    tracing::warn!(run = execution.run_id, %error, "worker artifact retained for retry");
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
                let request = journal.source(run, &key)?.context("unknown worker request")?;
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

    pub async fn serve(&self, listener: UnixListener, stamp: impl Fn() -> Result<v1::Stamp>) -> Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            // Bound the entire exchange, including idle clients and hub waits.
            match tokio::time::timeout(Duration::from_secs(5), self.exchange(stream, stamp()?)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "worker RPC rejected"),
                Err(error) => tracing::warn!(%error, "worker RPC deadline"),
            }
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
                if kind == boss_protocol::ProposalKind::RunDone {
                    self.capture_outputs(execution, stamp)?;
                }
                self.submit(execution, kind, payload, idempotency_key, stamp)
            }
            FrontendRequest::ListProposals {
                kind, state: filter, ..
            } => {
                ensure!(
                    filter.is_none(),
                    "hub_unavailable: authoritative proposal-state filtering requires the hub"
                );
                let journal = lock(&self.journal)?;
                let (_, state) = state(&journal, &run)?;
                let mut receipts: Vec<_> = state
                    .proposals
                    .into_iter()
                    .filter(|r| kind.is_none_or(|k| k == r.kind))
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
        if !refresh && let Some(bytes) = lock(&self.journal)?.source(run, &format!("worker-cache:{}", method as i32))? {
            let cached: Cached = serde_json::from_slice(&bytes)?;
            return Ok(response("cached", serde_json::to_value(cached)?));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let key = format!("worker-request:{id}");
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
            if let Some(bytes) = lock(&self.journal)?.source(run, &format!("worker-reply:{id}"))? {
                let value: Value = serde_json::from_slice(&bytes)?;
                ensure!(
                    value["code"].as_i64() == Some(v1::ResultCode::Ok as i64),
                    "hub_rejected: {}",
                    value["reason"]
                );
                return Ok(response("hub", value));
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "hub_unavailable: no current hub reply; request {id}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

use boss_engine_proposal_validation::{self as validation, ProposalCounts};
use boss_protocol::{ProposalKind, ProposalSubmissionError};

#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    pub id: String,
    pub kind: ProposalKind,
    pub digest: String,
    pub decision: Option<Value>,
}

impl Gateway {
    fn submit(
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
        let id = match key {
            Some(key) => {
                if let Err(field) = validation::validate_caller_idempotency_key(key.trim()) {
                    return Ok(FrontendEvent::ProposalRejected {
                        error: ProposalSubmissionError::validation(vec![field]),
                    });
                }
                ensure!(!key.trim().is_empty(), "idempotency key is empty");
                key.trim().to_owned()
            }
            None => validation::derive_idempotency_key(&execution.execution_id, kind, &validated.canonical_json),
        };
        let digest = boss_engine_attachments::store::digest_hex(validated.canonical_json.as_bytes());
        let run = identity(execution);
        let mut journal = lock(&self.journal)?;
        let (old, mut state) = state(&journal, &run)?;
        if let Some(receipt) = state.proposals.iter().find(|p| p.id == id) {
            ensure!(
                receipt.digest == digest && receipt.kind == kind,
                "idempotency key reused with different proposal"
            );
            let mut receipt = receipt.clone();
            receipt.decision = journal
                .source(&run, &decision_key(&receipt.id))?
                .map(|b| serde_json::from_slice(&b))
                .transpose()?;
            return Ok(receipt_event(&receipt, true));
        }
        if let Err(error) = validation::check_rate_caps(
            kind,
            ProposalCounts {
                total: state.proposals.len(),
                for_kind: state.proposals.iter().filter(|p| p.kind == kind).count(),
            },
        ) {
            return Ok(FrontendEvent::ProposalRejected { error });
        }
        // Preserve the exact validated frontend payload, including its kind;
        // the hub reuses the same validator and role authorization on replay.
        let canonical = serde_json::to_vec(
            &json!({"kind": kind, "payload": serde_json::from_str::<Value>(&validated.canonical_json)?, "idempotency_key": id}),
        )?;
        let watermark: Value = journal
            .source(&run, "transcript")?
            .map(|b| serde_json::from_slice(&b))
            .transpose()?
            .unwrap_or(Value::Null);
        let proposal = v1::Proposal {
            proposal_id: id.clone(),
            kind: wire_kind(kind) as i32,
            payload_contract: "proposal-json/1".into(),
            canonical_json: canonical,
            transcript_generation: watermark["cursor"]["generation"]
                .as_u64()
                .map(|generation| format!("{}-{generation}", watermark["incarnation"].as_u64().unwrap_or_default()))
                .unwrap_or_default(),
            transcript_through: watermark["cursor"]["offset"].as_u64().unwrap_or_default(),
        };
        proposal.validate()?;
        state.proposals.push(Receipt {
            id,
            kind,
            digest,
            decision: None,
        });
        journal.commit_source(
            &run,
            STATE,
            old.as_deref(),
            &serde_json::to_vec(&state)?,
            vec![(
                v1::Record {
                    run: Some(run.clone()),
                    observed: Some(stamp.clone()),
                    body: Some(record::Body::Proposal(proposal)),
                    ..Default::default()
                },
                Admission::Lifecycle,
            )],
        )?;
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

    fn open(root: &Path) -> Gateway {
        Gateway {
            journal: Arc::new(Mutex::new(
                Journal::open(&root.join("state"), "host", Limits::default()).unwrap(),
            )),
            state_dir: root.join("state"),
        }
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
        let gateway = Gateway {
            journal: Arc::new(Mutex::new(
                Journal::open(
                    &root.path().join("state"),
                    "host",
                    Limits {
                        spool_bytes: 4096,
                        lifecycle_reserve: 1024,
                    },
                )
                .unwrap(),
            )),
            state_dir: root.path().join("state"),
        };
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
}
