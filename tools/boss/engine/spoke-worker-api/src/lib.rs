//! Restricted worker IPC. Durable receipt never confers hub work authority.
//!
//! Command inventory. Every prompt source was searched for the commands a
//! worker is told to run: `execution-runner/src/runner/prompt.rs`, `prompt/`
//! and `prompt_fragments.rs` (including the CI-remediation, conflict-resolution
//! and progression fragments), `worker_setup.rs`, `review_prompt.rs`,
//! `review_guide_prompt.rs`, `progression_prompt.rs`, `answer_agent*.rs`,
//! `automation_triage.rs`, `ci_failure.rs` and `operator_question.rs`.
//!
//! `worker-api/1` accepts, as bound reads: context, PR status and PR body. It
//! accepts attachment registration and listing, and the existing validated
//! proposal kinds (done, wait, review reports/verdicts, blocked questions,
//! followups and PR-created). The run-scoped writes below are validated and
//! queued as records bound to the claimed run; the hub applies the same
//! ownership checks the local engine does, and none spends the proposal budget:
//! - `boss comment guide-outcome` (`guide-outcome/1`), one live outcome per
//!   comment;
//! - `boss comment reply` (`answer-reply/1`), the answer agent's one reply;
//! - `boss engine conflicts record-producer` (`conflict-producer/1`), in every
//!   worker's rules;
//! - `boss engine conflicts mark-failed` (`conflict-mark-failed/1`);
//! - `boss engine ci mark-failed` (`ci-mark-failed/1`).
//!
//! Listing returns this assignment's local receipts, explicitly labeled as
//! such. Context uses Spawn's sanitized bundle and context revision. Cache
//! misses and explicit refreshes emit WorkerRequest on the journal stream;
//! absence of a reply is hub_unavailable, and a reply that arrives after the
//! wait still fills the cache. Cached reads retain observation time.
//!
//! Commands that need an answer the hub must compute synchronously, or that are
//! administration verbs, are denied (as is every general app/board/control
//! method), so the execution kinds that depend on them are not composed for
//! remote dispatch (`compose_remote_spawn` rejects them): CI remediation, both
//! direct and revision runs with ci-fix provenance, needs
//! `engine ci mark-noop` (live-CI verdict), `mark-retriggered`, `classify` and
//! `mark-succeeded-via-rebase`; automation triage needs
//! `task create --automation`; progression supervision also uses arbitrary task
//! reads (`task show`), design-doc commands (`project design-doc`,
//! `project set-design-doc`), stuck reports (`propose unreachable`) and
//! `resolved merge`. Enabling such kinds remotely requires their own run-scoped
//! contracts. Product and project design likewise require `project set-design-doc`.
//! Review guides require host-local git store resolution.
//! Cube publication and gh remain host-local commands using host credentials.
//! The host-allocated PR/structured-output files are captured into durable
//! artifact sources and chunked over the same stream, including after restart.
//!
//! Spool lifecycle: registering an artifact stages its bytes plus a reserve for
//! the chunk records' overhead, so acceptance proves it can reach EOF. Each
//! emitted prefix moves into a numbered record in one transaction (the staged
//! blob is trimmed or released at the same time), so bytes are never counted
//! twice, even when bulk credit is smaller than a chunk. Request/reply rows are
//! deleted once consumed, and every remaining `worker-*` row is released when
//! the run is reclaimed and its artifacts are fully streamed.
//!
//! Output capture: at done, at an observed turn Stop and at exit, the host-
//! allocated output files are captured. A transient failure leaves a durable
//! pending marker that every tick retries (then an explicit gap), and reclaim
//! waits for the exit capture to conclude.
//!
//! Done and wait receipts permit a worker to finish its turn and park. Decisions
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
    /// Upper bound on what one chunk record carries beyond its data: run
    /// identity, artifact id, digest and offsets (the metadata is added to it).
    const RECORD_OVERHEAD: usize = 1024;
    /// Chunk records an artifact may need per staged chunk when bulk credit is
    /// smaller than a chunk and a chunk is emitted in more than one piece.
    const MIN_FRAGMENT: usize = CHUNK / 2;
    const RECORDS_PER_CHUNK: usize = CHUNK / MIN_FRAGMENT;
    /// A source checkpoint may not exceed 512 KiB.
    const MAX_RESERVE: usize = 400_000;
    /// Retries (one per observe tick) before an output that cannot be staged
    /// is reported to the hub as unavailable.
    const MAX_CAPTURE_ATTEMPTS: u32 = 120;

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

    /// Spool space set aside at registration for the chunk records' own
    /// overhead, so an accepted artifact never stalls for lack of room to
    /// describe its bytes. Each emitted record draws it down.
    fn reserve_key(id: &str) -> String {
        format!("worker-blob:{id}:reserve")
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
            // sequences ahead of lifecycle records. It includes a reserve for
            // the chunk records' own overhead, so admission here proves the
            // whole artifact can be emitted. The pump moves each staged prefix
            // into a record atomically, so the footprint never doubles. A
            // failed registration releases whatever it staged.
            metadata["registered_unix_ms"] = stamp.wall_unix_ms.into();
            let record_overhead = RECORD_OVERHEAD + serde_json::to_vec(&metadata)?.len();
            let reserve = bytes.len().div_ceil(CHUNK) * RECORDS_PER_CHUNK * record_overhead;
            ensure!(reserve <= MAX_RESERVE, "artifact metadata exceeds reservation limit");
            let mut staged = Vec::new();
            let reserved = std::iter::once((reserve_key(&id), vec![0u8; reserve])).chain(
                bytes
                    .chunks(CHUNK)
                    .enumerate()
                    .map(|(index, chunk)| (blob_key(&id, index), chunk.to_vec())),
            );
            for (key, value) in reserved {
                if let Err(error) = journal.commit_source(&run, &key, None, &value, vec![]) {
                    let _ = journal.delete_sources(&run, &staged);
                    return Err(error);
                }
                staged.push(key);
            }
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

        /// Deduplicate gap markers by output name within a completion boundary.
        /// Every boundary re-reads the file; registration deduplicates valid
        /// bytes by digest, so corrected files are never shadowed by this list.
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
            if gap.is_none() {
                // A successful read clears an earlier gap, so a later invalid
                // rewrite can report its own failure at the next boundary.
                state.capture.captured.retain(|n| n != name);
                return journal.commit_source(&run, STATE, old.as_deref(), &serde_json::to_vec(&state)?, vec![]);
            }
            if state.capture.captured.iter().any(|n| n == name) {
                return Ok(());
            }
            state.capture.captured.push(name.to_owned());
            let observations = gap
                .map(|reason| vec![gap_record(&run, stamp, format!("output:{name}"), reason)])
                .unwrap_or_default();
            journal.commit_source(&run, STATE, old.as_deref(), &serde_json::to_vec(&state)?, observations)
        }

        /// Capture host-allocated output paths recorded before launching the
        /// worker. Worker-supplied paths and symlinks never become output
        /// authority. Output problems never fail the caller: they become an
        /// explicit gap marker, or `Ok(true)` when something must be retried
        /// because the spool had no room. Only journal faults are returned.
        pub fn capture_outputs(&self, execution: &ExecutionRecord, stamp: &v1::Stamp) -> Result<bool> {
            let mut retry = false;
            for (name, path) in &execution.worker_outputs {
                if !matches!(name.as_str(), "BOSS_PR_URL_OUTPUT" | "BOSS_STRUCTURED_OUTPUT") {
                    continue;
                }
                match self.read_output(execution, name, path, stamp) {
                    Captured::Registered => self.conclude_output(execution, name, None, stamp)?,
                    Captured::Absent => {}
                    Captured::Transient(error) => {
                        tracing::warn!(run = execution.run_id, output = name, %error, "worker output retained for retry");
                        retry = true;
                    }
                    Captured::Invalid(reason) => self.conclude_output(execution, name, Some(reason), stamp)?,
                }
            }
            Ok(retry)
        }

        /// A completion boundary (done, turn Stop, exit): capture, and persist
        /// whether anything is still owed. A pending marker is retried on every
        /// upkeep tick independent of phase; after [`MAX_CAPTURE_ATTEMPTS`] the
        /// hub gets an explicit unavailable gap instead of silence. At exit the
        /// outcome also gates reclaim (see [`captures_concluded`]).
        pub(super) fn capture_boundary(
            &self,
            execution: &ExecutionRecord,
            stamp: &v1::Stamp,
            terminal: bool,
        ) -> Result<()> {
            let retry = self.capture_outputs(execution, stamp)?;
            let run = identity(execution);
            let mut journal = lock(&self.journal)?;
            let (old, mut state) = state(&journal, &run)?;
            let mut observations = Vec::new();
            if retry {
                state.capture.attempts += 1;
                if state.capture.attempts >= MAX_CAPTURE_ATTEMPTS {
                    observations.push(gap_record(
                        &run,
                        stamp,
                        "output:capture".into(),
                        format!(
                            "worker output unavailable: the spool had no room after {} attempts",
                            state.capture.attempts
                        ),
                    ));
                    state.capture.pending = false;
                    state.capture.attempts = 0;
                    state.capture.exited |= terminal;
                } else {
                    state.capture.pending = true;
                }
            } else {
                state.capture.pending = false;
                state.capture.attempts = 0;
                state.capture.exited |= terminal;
            }
            journal.commit_source(&run, STATE, old.as_deref(), &serde_json::to_vec(&state)?, observations)
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
            // The staged blob for this chunk always holds exactly the bytes not
            // yet emitted, so the unemitted tail is the whole source.
            let key = blob_key(&artifact.id, artifact.offset / CHUNK);
            let source = journal.source(run, &key)?.context("artifact source missing")?;
            let take = source.len().min(credit);
            if take == 0 || (take < MIN_FRAGMENT && take < source.len()) {
                return Ok(false);
            }
            let data = source[..take].to_vec();
            let metadata_json = serde_json::to_vec(&artifact.metadata)?;
            let overhead = RECORD_OVERHEAD + metadata_json.len();
            let finished = artifact.offset + take == artifact.size;
            let chunk = v1::ArtifactChunk {
                artifact_id: artifact.id.clone(),
                kind: artifact.kind,
                offset: artifact.offset as u64,
                sha256: Sha256::digest(&data).to_vec(),
                eof: finished,
                total_bytes: artifact.size as u64,
                metadata_json,
                data,
            };
            let reserve_key = reserve_key(&artifact.id);
            let reserve = journal.source(run, &reserve_key)?;
            artifact.offset += take;
            // One transaction moves the emitted prefix out of staging: a fully
            // emitted blob is released, a partly emitted one is trimmed to its
            // tail, and the record's overhead is paid from the reserve.
            let (mut released, mut trimmed) = (Vec::new(), Vec::new());
            if take == source.len() {
                released.push(key);
            } else {
                trimmed.push((key, source[take..].to_vec()));
            }
            if let Some(reserve) = reserve {
                if finished {
                    released.push(reserve_key);
                } else {
                    trimmed.push((reserve_key, vec![0u8; reserve.len().saturating_sub(overhead)]));
                }
            }
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
                boss_spoke_journal::Staging {
                    released: &released,
                    trimmed: &trimmed,
                },
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
    net::UnixStream,
};

mod server;

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
const MAX_GUIDE_BODY_BYTES: usize = 64 * 1024;
const MAX_WRITE_BODY_BYTES: usize = 64 * 1024;
/// Runaway bound on a run's run-scoped writes, far above any realistic count.
/// Guide outcomes are further bounded to one live outcome per comment.
const RUN_WRITE_CAP: usize = 512;
const MAX_CONFLICTED_FILES: usize = 500;

/// The run-scoped writes the gateway queues. Each is bound to the claimed run;
/// the hub applies the same ownership checks the local engine does. They are
/// bounded separately from proposals and never spend the proposal budget.
#[derive(Clone, Copy, PartialEq)]
enum RunWriteKind {
    GuideOutcome,
    AnswerReply,
    ConflictProducer,
    ConflictMarkFailed,
    CiMarkFailed,
}

impl RunWriteKind {
    const ALL: [Self; 5] = [
        Self::GuideOutcome,
        Self::AnswerReply,
        Self::ConflictProducer,
        Self::ConflictMarkFailed,
        Self::CiMarkFailed,
    ];

    /// The receipt label, which also tells listings what the receipt is.
    fn label(self) -> &'static str {
        match self {
            Self::GuideOutcome => "guide_outcome",
            Self::AnswerReply => "answer_reply",
            Self::ConflictProducer => "conflict_producer",
            Self::ConflictMarkFailed => "conflict_mark_failed",
            Self::CiMarkFailed => "ci_mark_failed",
        }
    }

    fn contract(self) -> &'static str {
        match self {
            Self::GuideOutcome => boss_protocol_spoke::proposal::GUIDE_OUTCOME,
            Self::AnswerReply => boss_protocol_spoke::proposal::ANSWER_REPLY,
            Self::ConflictProducer => boss_protocol_spoke::proposal::CONFLICT_PRODUCER,
            Self::ConflictMarkFailed => boss_protocol_spoke::proposal::CONFLICT_MARK_FAILED,
            Self::CiMarkFailed => boss_protocol_spoke::proposal::CI_MARK_FAILED,
        }
    }

    /// How many distinct receipts of this kind one run may hold. Guide outcomes
    /// are bounded per comment instead (see [`Self::supersedes`]).
    fn max_distinct(self) -> usize {
        match self {
            Self::GuideOutcome => RUN_WRITE_CAP,
            Self::AnswerReply => 1,
            Self::ConflictProducer => 64,
            Self::ConflictMarkFailed | Self::CiMarkFailed => 8,
        }
    }

    /// A later write with the same scope replaces the earlier one.
    fn supersedes(self) -> bool {
        self == Self::GuideOutcome
    }
}

fn is_run_write(label: &str) -> bool {
    RunWriteKind::ALL.iter().any(|kind| kind.label() == label)
}

/// A run-scoped write awaiting validation and queueing.
struct RunWrite {
    kind: RunWriteKind,
    /// Distinguishes writes of one kind (a comment or attempt id).
    scope: String,
    fields: Vec<ProposalFieldError>,
    canonical: Value,
}

#[derive(Clone)]
pub struct Gateway {
    pub journal: SharedJournal,
    pub state_dir: PathBuf,
    pub probe: TranscriptProbe,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    proposals: Vec<Receipt>,
    #[serde(default)]
    superseded: Vec<Receipt>,
    artifacts: Vec<artifacts::Artifact>,
    #[serde(default)]
    capture: CaptureState,
}

/// Where a run's output capture stands.
#[derive(Default, Serialize, Deserialize)]
struct CaptureState {
    /// Output names with an outstanding gap marker. A successful read or a
    /// rejected done reopens them; this never suppresses reading output bytes.
    captured: Vec<String>,
    /// A capture is owed (a turn ended or done was submitted) and must be
    /// retried on every upkeep tick until it concludes, whatever the phase.
    pending: bool,
    attempts: u32,
    /// The exit-time capture concluded. Reclaim waits for it.
    exited: bool,
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

fn gap_record(run: &v1::Run, stamp: &v1::Stamp, resource: String, reason: String) -> (v1::Record, Admission) {
    (
        v1::Record {
            run: Some(run.clone()),
            observed: Some(stamp.clone()),
            body: Some(record::Body::Gap(v1::Gap {
                resource,
                reason,
                ..Default::default()
            })),
            ..Default::default()
        },
        Admission::Lifecycle,
    )
}

/// A turn ended without `propose done` (a Stop observed by hook or transcript
/// ingestion). A worker may have written its outputs and parked, so the capture
/// is owed from here on and is retried until it concludes.
pub fn note_turn_stop(journal: &SharedJournal, run: &v1::Run) -> Result<()> {
    let mut journal = lock(journal)?;
    let (old, mut state) = state(&journal, run)?;
    if state.capture.pending {
        return Ok(());
    }
    state.capture.pending = true;
    journal.commit_source(run, STATE, old.as_deref(), &serde_json::to_vec(&state)?, vec![])
}

/// Whether the gateway has finished capturing this run's outputs: nothing is
/// pending and, once the worker has exited, the exit-time capture concluded.
/// Reclaim must wait for this, or a drain that completes between two gateway
/// ticks would discard output files that were never read.
pub fn captures_concluded(journal: &SharedJournal, record: &ExecutionRecord) -> Result<bool> {
    let (_, state) = state(&*lock(journal)?, &identity(record))?;
    Ok(!state.capture.pending && (record.phase != Phase::Terminal || state.capture.exited))
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
                self.upkeep(&execution, &stamp);
            }
        }
    }

    /// One run's share of a tick: capture owed outputs, stream artifacts and
    /// release a reclaimed run's rows. Faults are logged and retried next tick.
    fn upkeep(&self, execution: &ExecutionRecord, stamp: &v1::Stamp) {
        let run = identity(execution);
        // Reclaim waits for the exit capture, so an unreclaimed run is the only
        // one that can still owe output.
        if !execution.reclaimed {
            let exiting = matches!(execution.phase, Phase::Stopping | Phase::Terminal);
            let owed = lock(&self.journal)
                .and_then(|journal| state(&journal, &run))
                .map(|(_, state)| state.capture.pending || (exiting && !state.capture.exited));
            match owed {
                Ok(true) => {
                    if let Err(error) = self.capture_boundary(execution, stamp, execution.phase == Phase::Terminal) {
                        tracing::warn!(run = execution.run_id, %error, "worker output capture failed");
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(run = execution.run_id, %error, "worker output state unreadable"),
            }
        }
        if let Err(error) = self.pump(&run, stamp)
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
                // A request row is deleted once its reply is consumed. One whose
                // wait timed out leaves a per-method tombstone instead, so the
                // reply that arrives later still fills the cache; a replayed
                // reply, or one for an unknown request, has nothing to update.
                let open = journal.source(run, &key)?;
                let (method, late) = match &open {
                    Some(request) => (serde_json::from_slice::<i32>(request)?, None),
                    None => match late_method(&journal, run, &reply.request_id)? {
                        Some(found) => found,
                        None => {
                            tracing::debug!(request = reply.request_id, "worker reply for a closed request ignored");
                            return Ok(());
                        }
                    },
                };
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
                if let Some(tombstone) = late {
                    // Nobody is waiting: the cache holds the answer.
                    journal.delete_sources(run, &[tombstone])?;
                    return Ok(());
                }
                let old = journal.source(run, &source)?;
                ensure!(old.as_ref().is_none_or(|old| old == &bytes), "worker reply changed");
                journal.commit_source(run, &source, old.as_deref(), &bytes, vec![])?;
            }
            command::Body::ProposalDecision(decision) => {
                let (state_bytes, mut state) = state(&journal, run)?;
                let receipt = state
                    .proposals
                    .iter()
                    .chain(state.superseded.iter())
                    .find(|p| p.id == decision.proposal_id)
                    .context("unknown proposal")?;
                // A rejected done ends that completion boundary. The worker will
                // correct its outputs and finish again, and the corrected files
                // must be captured, not shadowed by the rejected attempt.
                let reopens = receipt.kind == ProposalKind::RunDone.as_str()
                    && decision.code != v1::ResultCode::Ok as i32
                    && !state.capture.captured.is_empty();
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
                if reopens {
                    state.capture.captured.clear();
                    journal.commit_source(run, STATE, state_bytes.as_deref(), &serde_json::to_vec(&state)?, vec![])?;
                }
            }
            _ => anyhow::bail!("unsupported worker command"),
        }
        Ok(())
    }

    async fn exchange(&self, stream: UnixStream, stamp: v1::Stamp) -> Result<()> {
        let credentials = stream.peer_cred()?;
        // One undecodable row must not deny every other worker its RPC.
        let records = lock(&self.journal)?
            .list_executions()?
            .into_iter()
            .filter_map(|bytes| match serde_json::from_slice::<ExecutionRecord>(&bytes) {
                Ok(record) => Some(record),
                Err(error) => {
                    tracing::warn!(%error, "worker gateway skipped an undecodable execution row");
                    None
                }
            })
            .collect::<Vec<_>>();
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
            | FrontendRequest::CommentsPostAnswer { run_id, .. }
            | FrontendRequest::ListAttachments { run_id } => run_id,
            FrontendRequest::RecordProducerSideConflict { execution_id, .. } => execution_id,
            // Attempt outcomes name an attempt, not a run: the caller is the
            // attributed process, and the hub checks the attempt is its own.
            FrontendRequest::MarkCiRemediationFailed { .. } | FrontendRequest::MarkConflictResolutionFailed { .. } => {
                &execution.execution_id
            }
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
            } => self.submit(execution, kind, payload, idempotency_key, stamp).await,
            FrontendRequest::CommentsRecordGuideOutcome {
                comment_id,
                disposition,
                body,
                request_regeneration,
                ..
            } => self.submit_guide_outcome(execution, comment_id, disposition, body, request_regeneration, stamp),
            FrontendRequest::CommentsPostAnswer { body, .. } => self.submit_answer_reply(execution, body, stamp),
            FrontendRequest::RecordProducerSideConflict {
                head_branch,
                base_branch,
                conflicted_files,
                ..
            } => self.submit_producer_conflict(execution, head_branch, base_branch, conflicted_files, stamp),
            FrontendRequest::MarkConflictResolutionFailed {
                attempt_id,
                reason,
                evidence,
            } => self.submit_attempt_failure(
                execution,
                RunWriteKind::ConflictMarkFailed,
                attempt_id,
                reason,
                json!({"evidence": evidence}),
                stamp,
            ),
            FrontendRequest::MarkCiRemediationFailed {
                attempt_id,
                reason,
                queue_ci_evidence,
            } => self.submit_attempt_failure(
                execution,
                RunWriteKind::CiMarkFailed,
                attempt_id,
                reason,
                json!({"queue_ci_evidence": queue_ci_evidence}),
                stamp,
            ),
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
        if lock(&self.journal)?.source(run, &late_key(method))?.is_some() {
            anyhow::bail!("hub_unavailable: previous request is still outstanding");
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
                // The wait is over, but a reply that arrives later is still worth
                // caching. One tombstone per method bounds what this leaves behind.
                let mut journal = lock(&self.journal)?;
                if journal.source(run, &reply_key)?.is_some() {
                    continue;
                }
                let tombstone = late_key(method);
                let previous = journal.source(run, &tombstone)?;
                journal.commit_source(run, &tombstone, previous.as_deref(), id.as_bytes(), vec![])?;
                journal.delete_sources(run, &[key])?;
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
    /// The wire kind and payload contract the hub dispatches on.
    wire: (v1::ProposalKind, &'static str),
    canonical: Vec<u8>,
    /// Receipts whose id starts with this are replaced by this one.
    supersedes: Option<String>,
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
        let canonical = serde_json::to_vec(&boss_protocol_spoke::proposal::ProposalJsonEnvelope {
            kind,
            payload: serde_json::from_str::<Value>(&validated.canonical_json)?,
            idempotency_key: id.clone(),
        })?;
        // What the transcript source is right now, never the ingestion cursor:
        // a lagging or credit-starved tail must not understate completion
        // evidence. Resolved before the journal lock is taken.
        let watermark = (self.probe)(execution.clone()).await;
        let queued = Queued {
            id,
            label: kind.as_str().to_owned(),
            wire: (
                boss_protocol_spoke::proposal::wire_kind(kind),
                boss_protocol_spoke::proposal::PROPOSAL_JSON,
            ),
            canonical,
            supersedes: None,
        };
        if kind == ProposalKind::RunDone {
            {
                let journal = lock(&self.journal)?;
                let (_, state) = state(&journal, &identity(execution))?;
                if !state.proposals.iter().any(|p| p.id == queued.id)
                    && let Err(error) = validation::check_rate_caps(
                        kind,
                        ProposalCounts {
                            total: state.proposals.iter().filter(|p| !is_run_write(&p.kind)).count(),
                            for_kind: state.proposals.iter().filter(|p| p.kind == kind.as_str()).count(),
                        },
                    )
                {
                    return Ok(FrontendEvent::ProposalRejected { error });
                }
            }
            if let Err(error) = self.capture_boundary(execution, stamp, false) {
                tracing::warn!(run = execution.run_id, %error, "worker output capture failed at done");
                note_turn_stop(&self.journal, &identity(execution))?;
            }
        }
        self.enqueue(
            execution,
            queued,
            |state| {
                // Run-scoped writes have their own bounds; they never spend the
                // proposal budget.
                validation::check_rate_caps(
                    kind,
                    ProposalCounts {
                        total: state.proposals.iter().filter(|p| !is_run_write(&p.kind)).count(),
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
    /// Re-recording a comment supersedes its earlier outcome.
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
        self.submit_run_write(
            execution,
            RunWrite {
                kind: RunWriteKind::GuideOutcome,
                scope: comment_id.clone(),
                fields,
                canonical: boss_protocol_spoke::proposal::guide_outcome_body(
                    &execution.execution_id,
                    &comment_id,
                    &disposition,
                    &body,
                    request_regeneration,
                ),
            },
            stamp,
        )
    }

    /// `boss comment reply`: the answer agent's one write. The hub binds it to
    /// the run's own comment, as the local engine does.
    fn submit_answer_reply(
        &self,
        execution: &ExecutionRecord,
        body: String,
        stamp: &v1::Stamp,
    ) -> Result<FrontendEvent> {
        let mut fields = Vec::new();
        if body.trim().is_empty() {
            fields.push(ProposalFieldError::new("body", "reply body may not be empty"));
        }
        if body.len() > MAX_WRITE_BODY_BYTES {
            fields.push(ProposalFieldError::new(
                "body",
                format!("reply body is over the {MAX_WRITE_BODY_BYTES}-byte limit"),
            ));
        }
        self.submit_run_write(
            execution,
            RunWrite {
                kind: RunWriteKind::AnswerReply,
                scope: String::new(),
                fields,
                canonical: boss_protocol_spoke::proposal::answer_reply_body(&execution.execution_id, &body),
            },
            stamp,
        )
    }

    /// `boss engine conflicts record-producer`: telemetry about a conflict the
    /// worker already resolved inline.
    fn submit_producer_conflict(
        &self,
        execution: &ExecutionRecord,
        head_branch: String,
        base_branch: String,
        conflicted_files: Vec<String>,
        stamp: &v1::Stamp,
    ) -> Result<FrontendEvent> {
        let mut fields = Vec::new();
        for (name, value) in [("head_branch", &head_branch), ("base_branch", &base_branch)] {
            if value.trim().is_empty() || value.len() > 512 {
                fields.push(ProposalFieldError::new(
                    name,
                    "branch must be non-empty and at most 512 bytes",
                ));
            }
        }
        if conflicted_files.len() > MAX_CONFLICTED_FILES || conflicted_files.iter().any(|f| f.len() > 1024) {
            fields.push(ProposalFieldError::new(
                "conflicted_files",
                format!("at most {MAX_CONFLICTED_FILES} paths of at most 1024 bytes"),
            ));
        }
        self.submit_run_write(
            execution,
            RunWrite {
                kind: RunWriteKind::ConflictProducer,
                scope: String::new(),
                fields,
                canonical: boss_protocol_spoke::proposal::conflict_producer_body(
                    &execution.execution_id,
                    &head_branch,
                    &base_branch,
                    conflicted_files,
                ),
            },
            stamp,
        )
    }

    /// `boss engine conflicts mark-failed` and `boss engine ci mark-failed`:
    /// a terminal outcome for an attempt. The hub resolves the attempt and
    /// accepts it only if it belongs to this run.
    fn submit_attempt_failure(
        &self,
        execution: &ExecutionRecord,
        kind: RunWriteKind,
        attempt_id: String,
        reason: String,
        extra: Value,
        stamp: &v1::Stamp,
    ) -> Result<FrontendEvent> {
        let mut fields = Vec::new();
        if attempt_id.trim().is_empty() || attempt_id.len() > 256 {
            fields.push(ProposalFieldError::new(
                "attempt_id",
                "attempt id must be non-empty and at most 256 bytes",
            ));
        }
        if reason.trim().is_empty() || reason.len() > 1024 {
            fields.push(ProposalFieldError::new(
                "reason",
                "reason must be non-empty and at most 1024 bytes",
            ));
        }
        if kind == RunWriteKind::ConflictMarkFailed
            && let Err(message) = boss_protocol::validate_conflict_mark_failed_reason(&reason)
        {
            fields.push(ProposalFieldError::new("reason", message));
        }
        if kind == RunWriteKind::ConflictMarkFailed && reason == boss_protocol::CONFLICT_ARCHITECTURAL_MISMATCH_REASON {
            let evidence =
                serde_json::from_value::<boss_protocol::ArchitecturalMismatchEvidence>(extra["evidence"].clone())
                    .map_err(|error| format!("architectural mismatch evidence required: {error}"))
                    .and_then(|evidence| evidence.validate());
            if let Err(message) = evidence {
                fields.push(ProposalFieldError::new("evidence", message));
            }
        }
        self.submit_run_write(
            execution,
            RunWrite {
                kind,
                scope: attempt_id.clone(),
                fields,
                canonical: match kind {
                    RunWriteKind::CiMarkFailed => boss_protocol_spoke::proposal::ci_mark_failed_body(
                        &execution.execution_id,
                        &attempt_id,
                        &reason,
                        extra,
                    ),
                    _ => boss_protocol_spoke::proposal::conflict_mark_failed_body(
                        &execution.execution_id,
                        &attempt_id,
                        &reason,
                        extra,
                    ),
                },
            },
            stamp,
        )
    }

    /// Validate and queue one run-scoped write as a durable record bound to the
    /// claimed run. Identical retries replay; none of it spends the proposal
    /// budget.
    fn submit_run_write(
        &self,
        execution: &ExecutionRecord,
        write: RunWrite,
        stamp: &v1::Stamp,
    ) -> Result<FrontendEvent> {
        if !write.fields.is_empty() {
            return Ok(FrontendEvent::ProposalRejected {
                error: ProposalSubmissionError::validation(write.fields),
            });
        }
        let canonical = serde_json::to_vec(&write.canonical)?;
        let digest = boss_engine_attachments::store::digest_hex(&canonical);
        // Hashed so one scope can never be a prefix of another.
        let label = write.kind.label();
        let stem = format!(
            "{label}:{}:",
            &boss_engine_attachments::store::digest_hex(write.scope.as_bytes())[..16]
        );
        let queued = Queued {
            id: format!("{stem}{}", &digest[..16]),
            label: label.into(),
            wire: (v1::ProposalKind::OtherValidated, write.kind.contract()),
            canonical,
            supersedes: write.kind.supersedes().then(|| stem.clone()),
        };
        let limit = write.kind.max_distinct();
        self.enqueue(
            execution,
            queued,
            move |state| {
                let for_label = state.proposals.iter().filter(|p| p.kind == label).count();
                let total = state.proposals.iter().filter(|p| is_run_write(&p.kind)).count();
                if for_label >= limit || total >= RUN_WRITE_CAP {
                    return Err(ProposalSubmissionError::new(
                        ProposalErrorCode::RateLimited,
                        format!("this execution has reached its {label} submission cap"),
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
        if let Some(receipt) = state
            .proposals
            .iter()
            .chain(state.superseded.iter())
            .find(|p| p.id == queued.id)
        {
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
        if let Some(stem) = &queued.supersedes {
            if state.superseded.len() >= RUN_WRITE_CAP {
                return Ok(FrontendEvent::ProposalRejected {
                    error: ProposalSubmissionError::new(
                        ProposalErrorCode::RateLimited,
                        "superseded receipt cap reached",
                    ),
                });
            }
            let (superseded, current): (Vec<_>, Vec<_>) = state
                .proposals
                .into_iter()
                .partition(|p| p.id.starts_with(stem.as_str()));
            state.superseded.extend(superseded);
            state.proposals = current;
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
            kind: queued.wire.0 as i32,
            payload_contract: queued.wire.1.into(),
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

const READ_METHODS: [v1::WorkerMethod; 3] = [
    v1::WorkerMethod::RunContext,
    v1::WorkerMethod::PrStatus,
    v1::WorkerMethod::PrBody,
];

/// Names the request whose wait timed out, per method.
fn late_key(method: v1::WorkerMethod) -> String {
    format!("worker-late:{}", method as i32)
}

/// The method of a timed-out request and its tombstone row, if `request_id` is
/// the one still remembered for that method.
fn late_method(journal: &Journal, run: &v1::Run, request_id: &str) -> Result<Option<(i32, Option<String>)>> {
    for method in READ_METHODS {
        let key = late_key(method);
        if journal.source(run, &key)?.as_deref() == Some(request_id.as_bytes()) {
            return Ok(Some((method as i32, Some(key))));
        }
    }
    Ok(None)
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

#[cfg(test)]
mod tests;
