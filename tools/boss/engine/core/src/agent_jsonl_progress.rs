//! Run-correlated agent JSONL file ingress.
//!
//! Pane-hosted workers do not give the engine their pty master. A driver that
//! declares [`crate::driver::ProgressIngress::AgentJsonlFile`] instead writes
//! raw JSONL into a run-private directory. This module discovers exactly one
//! new, workspace-correlated file per run and exposes its growing bytes as an
//! [`tokio::io::AsyncRead`] stream to [`crate::stdout_progress`]'s existing
//! generic JSONL reader.
//!
//! The file writer is never coupled to the reader: bounded backpressure stops
//! disk reads at the duplex boundary while the agent keeps appending normally.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, watch};

pub use crate::agent_jsonl_discovery::FileIdentity;
use crate::agent_jsonl_discovery::{
    Candidate, PreparedSource, RolloutProbeSource, RolloutProbeTarget, StreamHalt, descriptor_is_unlinked,
    file_identity, named_descriptor_matches, single_link_regular, validate_candidate, validated_session_meta,
};
use crate::driver::{AgentDriver, AgentJsonlFileIngress, ProgressSessionConfig, ProgressStreamSource};
use crate::stdout_progress::{ProgressCheckpointSink, WorkerEventSink};

const FILE_POLL: Duration = Duration::from_millis(50);
/// How long discovery may run before the run is reported as *overdue*.
///
/// This is a reporting threshold, not a give-up point. It used to be the
/// latter (`DISCOVERY_TIMEOUT`): discovery returned an error after 120s and
/// the ingress task exited, leaving the run permanently unobserved — no
/// events, no `transcript_path`, no driver-start signal — even when the
/// rollout appeared seconds later. During the 2026-09-13 admission burst
/// four silent Codex workers' rollouts appeared 123–126s after activation,
/// consistent with missing this window; a fifth appeared at 76s and remains
/// unexplained. One late worker completed before the reap. The driver-
/// start sweep ([`crate::live_worker_state::DRIVER_START_GRACE_SECS`]) is
/// the liveness authority for a spawn that never reports; discovery does not
/// get a shorter clock of its own. It now runs until the run is torn down
/// ([`AgentJsonlProgressManager::stop_run`]) and merely *records* — durably,
/// on the checkpoint, and as a dispatch event — that it is overdue.
/// The threshold also slows scans from 100ms to one second.
pub const DISCOVERY_OVERDUE_AFTER: Duration = crate::agent_jsonl_discovery::DISCOVERY_TIMEOUT;
const FILE_CHUNK_BYTES: usize = 64 * 1024;
const DUPLEX_BYTES: usize = 64 * 1024;

/// Where a run's file ingress had got to, durably.
///
/// Written by the ingress itself and read back by
/// [`crate::app::ServerState::readopt_live_worker`]. It exists because the
/// two answers available without it are both wrong for a session that
/// outlives an engine restart: re-tailing from byte 0 republishes every
/// record of every prior turn — a second `SessionStart`, a second `Stop` per
/// turn, every tool call again — and starting at end-of-file discards
/// whatever the worker wrote while the engine was down, which for a turn that
/// ended during the restart is the turn boundary itself.
///
/// Every variant is written by [`AgentJsonlProgressManager`] on the spawn
/// path, so its *absence* is itself information: it means no engine ever
/// armed an ingress for this run.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IngressCheckpoint {
    /// The run's driver does not tail a file — its progress arrives over the
    /// hook socket or its own stdout. Recorded rather than omitted so
    /// readoption can tell "nothing to re-establish" apart from "the record
    /// is missing and I cannot tell".
    NotFileIngress,
    /// A file ingress was armed at spawn but no correlated rollout had been
    /// attached yet. `baseline` is the pre-spawn snapshot discovery diffs
    /// against; without it a re-armed discovery would accept a rollout that
    /// already existed before this run started.
    Armed {
        ingress: AgentJsonlFileIngress,
        baseline: Vec<PathBuf>,
        /// What discovery had to say for itself while still unattached: the
        /// most recent overdue notice or the failure that ended it. Absent
        /// until discovery has run past [`DISCOVERY_OVERDUE_AFTER`] or
        /// failed. This is what turns the post-hoc question "did the driver
        /// never start, or did the engine never look?" into a read of the
        /// run row instead of a grep through a rotated trace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        discovery: Option<DiscoveryRecord>,
    },
    /// Attached to exactly one rollout and consumed through `consumed_bytes`.
    ///
    /// `consumed_bytes` is a byte offset into `path`, always immediately past
    /// a newline, and always a position whose events have already been
    /// through the engine's fan-out. `session_state` is the driver session
    /// that belongs to that same offset — see
    /// [`crate::driver::ProgressSessionNormalizer::resume_state`].
    ///
    /// `identity` names the *incarnation* of `path` that offset is an offset
    /// into. A path is not an identity: the rollout can be rotated or
    /// replaced under the same name (the live tail already handles that case
    /// mid-stream), and if the engine dies before any byte of the new
    /// incarnation has been dispatched, the stored offset still describes the
    /// dead one. Resuming on path alone would then attach at an offset
    /// belonging to a different file — skipping its first `consumed_bytes`
    /// bytes and most likely landing mid-line. Recording the device/inode the
    /// offset came from turns that into the same loud failure as a vanished
    /// or uncorrelated rollout.
    Attached {
        ingress: AgentJsonlFileIngress,
        path: PathBuf,
        session_id: String,
        consumed_bytes: u64,
        identity: FileIdentity,
        #[serde(default)]
        session_state: Option<serde_json::Value>,
    },
}

/// Discovery's verdict on an ingress that has not attached.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryVerdict {
    /// Past [`DISCOVERY_OVERDUE_AFTER`] and still looking.
    Overdue,
    /// Discovery ended without attaching; see the record's `reason`.
    Failed,
}

/// A durable note from discovery, stored on [`IngressCheckpoint::Armed`].
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct DiscoveryRecord {
    pub verdict: DiscoveryVerdict,
    /// Wall-clock epoch seconds when the record was written.
    pub at_epoch_secs: i64,
    /// How long discovery had been running when the record was written,
    /// measured from activation (the spawn acknowledgement), not arming.
    pub waited_secs: u64,
    /// Rollout-shaped files that did not correlate to this run on the latest scan.
    pub rejected_candidates: usize,
    /// Bounded examples, including transient incomplete metadata.
    #[serde(default)]
    #[builder(default)]
    pub rejections: Vec<CandidateRejection>,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CandidateRejection {
    pub file_name: String,
    pub reason: CandidateRejectReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateRejectReason {
    UnsafeFile,
    OutsideRoot,
    IdentityChanged,
    IncompleteSessionMeta,
    OversizedSessionMeta,
    InvalidSessionMeta,
    SessionIdMismatch,
    WorkspaceMismatch,
    FilenameMismatch,
    IoOrParseError,
}

/// A file-ingress lifecycle milestone, reported through
/// [`crate::stdout_progress::WorkerEventSink::record_ingress_observation`]
/// so the engine can put it on the run's dispatch timeline.
///
/// The ingress cannot emit a dispatch event itself — it has a sink and a
/// checkpoint store, not the engine — and threading a third handle through
/// every constructor for one line of telemetry is worse than one more
/// method on the sink it already holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IngressObservation {
    /// Discovery identified this run's rollout.
    Attached {
        path: PathBuf,
        session_id: String,
        /// Seconds between activation and attachment. `None` when the run
        /// was re-adopted rather than discovered — it was attached before
        /// this engine existed.
        discovery_secs: Option<u64>,
    },
    /// Discovery has run past [`DISCOVERY_OVERDUE_AFTER`] without attaching
    /// and is still running.
    DiscoveryOverdue {
        root: PathBuf,
        waited_secs: u64,
        rejected_candidates: usize,
    },
    /// Discovery ended without attaching.
    DiscoveryFailed {
        root: PathBuf,
        waited_secs: u64,
        rejected_candidates: usize,
        reason: String,
    },
}

/// Engine-owned durable storage for [`IngressCheckpoint`].
///
/// A seam, in the same shape and for the same reason as
/// [`crate::driver::ProgressIdentityStore`]: the resume point must not live
/// in an agent-writable home, and the ingress must not depend on the whole
/// engine to write it.
pub trait IngressCheckpointStore: Send + Sync {
    fn store_ingress_checkpoint(&self, run_id: &str, checkpoint: &IngressCheckpoint) -> Result<(), String>;
    fn load_ingress_checkpoint(&self, run_id: &str) -> Result<Option<IngressCheckpoint>, String>;

    /// Resolve, once, where this run's repeated checkpoint writes go.
    ///
    /// The steady-state cost of the durability guarantee is one write per
    /// dispatched event, so anything else the write path does is paid at that
    /// same rate. The production store derives the `work_runs` row from the
    /// execution id with an ordered scan, under the process-wide work-db
    /// connection lock; that answer is fixed for the life of a run, so the
    /// ingress resolves it at attach time and hands it back on every write.
    ///
    /// The default is no pre-resolution: a store keyed directly by run id has
    /// nothing to resolve.
    fn resolve_checkpoint_target(&self, run_id: &str) -> Result<CheckpointTarget, String> {
        Ok(CheckpointTarget::ByRunId(run_id.to_owned()))
    }

    /// Write to a destination from [`Self::resolve_checkpoint_target`].
    fn store_ingress_checkpoint_at(
        &self,
        target: &CheckpointTarget,
        checkpoint: &IngressCheckpoint,
    ) -> Result<(), String> {
        match target {
            CheckpointTarget::ByRunId(run_id) => self.store_ingress_checkpoint(run_id, checkpoint),
            CheckpointTarget::Resolved(handle) => {
                Err(format!("this store did not resolve the checkpoint target {handle}"))
            }
        }
    }
}

/// Where an [`IngressCheckpointStore`] writes one run's checkpoints.
#[derive(Clone, Debug)]
pub enum CheckpointTarget {
    /// The store looks its destination up from the run id on each write.
    /// What a store with nothing to pre-resolve returns, and the fallback
    /// when pre-resolution fails.
    ByRunId(String),
    /// A store-private handle resolved once at attach time. Opaque: only the
    /// store that produced it may interpret it.
    Resolved(String),
}

impl RolloutProbeSource for crate::work::WorkDb {
    fn load_rollout_probe_target(&self, run_id: &str) -> Result<Option<RolloutProbeTarget>, String> {
        Ok(match self.load_ingress_checkpoint(run_id)? {
            None => None,
            Some(IngressCheckpoint::NotFileIngress) => Some(RolloutProbeTarget::NotFileIngress),
            Some(IngressCheckpoint::Armed { ingress, baseline, .. }) => {
                Some(RolloutProbeTarget::Armed { ingress, baseline })
            }
            Some(IngressCheckpoint::Attached { ingress, path, .. }) => {
                Some(RolloutProbeTarget::Attached { ingress, path })
            }
        })
    }
}

impl IngressCheckpointStore for crate::work::WorkDb {
    fn store_ingress_checkpoint(&self, run_id: &str, checkpoint: &IngressCheckpoint) -> Result<(), String> {
        let json = serde_json::to_string(checkpoint).map_err(|err| format!("{err}"))?;
        self.set_run_progress_ingress_checkpoint(run_id, &json)
            .map_err(|err| format!("{err:#}"))
    }

    fn resolve_checkpoint_target(&self, run_id: &str) -> Result<CheckpointTarget, String> {
        self.resolve_run_row_for_execution(run_id)
            .map_err(|err| format!("{err:#}"))?
            .map(CheckpointTarget::Resolved)
            .ok_or_else(|| format!("no work_runs row for execution {run_id}"))
    }

    fn store_ingress_checkpoint_at(
        &self,
        target: &CheckpointTarget,
        checkpoint: &IngressCheckpoint,
    ) -> Result<(), String> {
        match target {
            CheckpointTarget::ByRunId(run_id) => self.store_ingress_checkpoint(run_id, checkpoint),
            CheckpointTarget::Resolved(run_row_id) => {
                let json = serde_json::to_string(checkpoint).map_err(|err| format!("{err}"))?;
                self.set_run_progress_ingress_checkpoint_by_row(run_row_id, &json)
                    .map_err(|err| format!("{err:#}"))
            }
        }
    }

    fn load_ingress_checkpoint(&self, run_id: &str) -> Result<Option<IngressCheckpoint>, String> {
        let Some(json) = self
            .get_run_progress_ingress_checkpoint(run_id)
            .map_err(|err| format!("{err:#}"))?
        else {
            return Ok(None);
        };
        serde_json::from_str(&json)
            .map(Some)
            .map_err(|err| format!("stored ingress checkpoint is not readable: {err}"))
    }
}

/// One contiguous run of bytes shared by the reader's stream and the rollout
/// file it came from.
///
/// `stream_len` and `file_len` differ only for the synthetic newline the tail
/// injects when the file is truncated or rotated: that byte exists in the
/// stream (it forces a line boundary so an old fragment cannot glue onto a
/// new first line) and corresponds to no byte of any file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StreamSegment {
    stream_start: u64,
    stream_len: u64,
    file_start: u64,
    file_len: u64,
    /// The incarnation these file offsets are offsets into. Per segment
    /// rather than per map because the reader can still be inside a segment
    /// that predates a rotation the tail has already installed — and a
    /// checkpoint taken there must name the incarnation the *offset* belongs
    /// to, not the one the tail happens to be reading now.
    identity: FileIdentity,
}

/// Translates the reader's consumed-byte position back into a rollout-file
/// offset.
///
/// The two positions are usually the same number, and it would be tempting to
/// treat them as one. They are not: the tail resumes at a non-zero file
/// offset after a readoption, and truncation or same-path rotation resets the
/// file offset to zero mid-stream. Recording the correspondence explicitly is
/// what keeps a checkpoint from naming a byte in a file incarnation that no
/// longer exists.
#[derive(Debug, Default)]
struct StreamFileMap {
    segments: VecDeque<StreamSegment>,
    stream_end: u64,
}

/// A resolved point in the rollout: which byte, and which incarnation of the
/// file that byte belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FilePosition {
    offset: u64,
    identity: FileIdentity,
}

impl StreamFileMap {
    /// Note that `len` bytes starting at `file_start` are about to be handed
    /// to the reader. Called *before* the write so the map is never behind
    /// bytes the reader could already have consumed.
    fn record_bytes(&mut self, file_start: u64, len: u64, identity: FileIdentity) {
        if len == 0 {
            return;
        }
        if let Some(last) = self.segments.back_mut()
            && last.identity == identity
            && last.stream_len == last.file_len
            && last.stream_start + last.stream_len == self.stream_end
            && last.file_start + last.file_len == file_start
        {
            last.stream_len += len;
            last.file_len += len;
            self.stream_end += len;
            return;
        }
        self.segments.push_back(StreamSegment {
            stream_start: self.stream_end,
            stream_len: len,
            file_start,
            file_len: len,
            identity,
        });
        self.stream_end += len;
    }

    /// Note the one-byte synthetic line delimiter, after which the file
    /// offset restarts at `next_file_start` in incarnation `identity`.
    fn record_delimiter(&mut self, next_file_start: u64, identity: FileIdentity) {
        self.segments.push_back(StreamSegment {
            stream_start: self.stream_end,
            stream_len: 1,
            file_start: next_file_start,
            file_len: 0,
            identity,
        });
        self.stream_end += 1;
    }

    /// The rollout-file position the reader is at, having consumed
    /// `stream_offset` bytes — the offset and the incarnation it indexes.
    ///
    /// Resolves against the newest segment that starts at or before the
    /// position, so a position sitting exactly on an incarnation boundary
    /// names the new incarnation rather than the dead one.
    fn file_position_for(&self, stream_offset: u64) -> Option<FilePosition> {
        self.segments
            .iter()
            .rev()
            .find(|segment| segment.stream_start <= stream_offset)
            .map(|segment| FilePosition {
                offset: segment.file_start + (stream_offset - segment.stream_start).min(segment.file_len),
                identity: segment.identity,
            })
    }

    /// Drop segments the reader can never ask about again.
    fn prune_through(&mut self, stream_offset: u64) {
        while self.segments.len() > 1
            && self
                .segments
                .get(1)
                .is_some_and(|next| next.stream_start <= stream_offset)
        {
            self.segments.pop_front();
        }
    }
}

/// Writes the run's [`IngressCheckpoint::Attached`] record after every
/// dispatched event.
///
/// The *write* is per event by design — that is what bounds the crash window
/// to a single record — but nothing else here may be. `target` is the store's
/// resolved destination for this run, resolved once when the ingress attaches
/// rather than re-derived on every event: on the production store that
/// derivation is a scan over `work_runs` under the single shared work-db
/// connection lock, for an answer that cannot change across a run.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct AttachedCheckpointer {
    run_id: String,
    store: Arc<dyn IngressCheckpointStore>,
    target: CheckpointTarget,
    ingress: AgentJsonlFileIngress,
    path: PathBuf,
    session_id: String,
    map: Arc<Mutex<StreamFileMap>>,
}

impl ProgressCheckpointSink for AttachedCheckpointer {
    fn record_progress_checkpoint(&self, consumed_bytes: u64, session_state: Option<serde_json::Value>) {
        let Ok(mut map) = self.map.lock() else {
            tracing::warn!(
                run_id = %self.run_id,
                "agent JSONL progress: offset map poisoned; leaving the resume point where it was",
            );
            return;
        };
        let Some(position) = map.file_position_for(consumed_bytes) else {
            // Cannot happen while the tail is the only writer — the reader
            // consumes bytes the tail recorded first. Leaving the stored
            // point alone re-reads a bounded prefix on resume, which is the
            // safe direction; skipping ahead would drop records.
            tracing::warn!(
                run_id = %self.run_id,
                consumed_bytes,
                "agent JSONL progress: no file offset for the reader position; resume point unchanged",
            );
            return;
        };
        map.prune_through(consumed_bytes);
        drop(map);
        let checkpoint = IngressCheckpoint::Attached {
            ingress: self.ingress.clone(),
            path: self.path.clone(),
            session_id: self.session_id.clone(),
            consumed_bytes: position.offset,
            identity: position.identity,
            session_state,
        };
        if let Err(err) = self.store.store_ingress_checkpoint_at(&self.target, &checkpoint) {
            tracing::warn!(
                run_id = %self.run_id,
                file_offset = position.offset,
                %err,
                "agent JSONL progress: could not persist the resume point",
            );
        }
    }
}

/// How an ingress task gets hold of the rollout it is going to read.
enum IngressStart {
    /// Ordinary spawn: watch for the one new correlated rollout to appear.
    Discover,
    /// Readoption: attach to the exact rollout the previous engine was
    /// reading, at the exact byte it had consumed through.
    Resume {
        candidate: Candidate,
        file_offset: u64,
        session_state: Option<serde_json::Value>,
    },
}

struct RunHandle {
    activate: Option<oneshot::Sender<()>>,
    halt: watch::Sender<StreamHalt>,
}

/// What [`AgentJsonlProgressManager::resume_run`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeOutcome {
    /// The run's ingress is live again and tailing from the recorded byte.
    Reestablished,
    /// The run's driver never had a file ingress to re-establish.
    NotFileIngress,
}

/// Owns at most one prepared/active file ingress per execution id.
pub struct AgentJsonlProgressManager {
    runs: Mutex<HashMap<String, RunHandle>>,
    /// See [`DISCOVERY_OVERDUE_AFTER`]. A field so a test can drive the
    /// overdue path in milliseconds rather than minutes.
    discovery_overdue_after: Duration,
}

impl Default for AgentJsonlProgressManager {
    fn default() -> Self {
        Self {
            runs: Mutex::new(HashMap::new()),
            discovery_overdue_after: DISCOVERY_OVERDUE_AFTER,
        }
    }
}

impl AgentJsonlProgressManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Report discovery as overdue after `after` instead of
    /// [`DISCOVERY_OVERDUE_AFTER`]. Reporting only: discovery keeps running
    /// either way.
    #[cfg(test)]
    fn with_discovery_overdue_after(mut self, after: Duration) -> Self {
        self.discovery_overdue_after = after;
        self
    }

    /// Snapshot pre-existing candidates before the pane is spawned, then
    /// prepare a task that waits for [`Self::activate_run`].
    ///
    /// Records the snapshot as this run's [`IngressCheckpoint::Armed`] point
    /// before the task exists, so an engine that dies between here and the
    /// rollout appearing can still re-arm discovery against the right
    /// baseline.
    pub fn prepare_run<S>(
        &self,
        run_id: &str,
        driver: std::sync::Arc<dyn AgentDriver>,
        ingress: AgentJsonlFileIngress,
        sink: S,
        store: Arc<dyn IngressCheckpointStore>,
    ) -> Result<(), String>
    where
        S: WorkerEventSink + Send + Sync + 'static,
    {
        let prepared = PreparedSource::new(ingress)?;
        let armed = IngressCheckpoint::Armed {
            ingress: prepared.ingress.clone(),
            baseline: prepared.baseline.iter().cloned().collect(),
            discovery: None,
        };
        store.store_ingress_checkpoint(run_id, &armed)?;
        self.spawn_ingress(run_id, driver, prepared, sink, store, IngressStart::Discover)
    }

    /// Re-establish an ingress the engine had already armed, from the durable
    /// record of where it got to.
    ///
    /// Fallible and synchronous on purpose. Everything that can go wrong here
    /// — the rollout is gone, it no longer correlates to this run, it is
    /// shorter than the offset we consumed — is a condition an operator has to
    /// be told about, and a background task that discovered it would have
    /// nobody to tell. The caller reports; this never attaches "somewhere
    /// near" the recorded point.
    pub fn resume_run<S>(
        &self,
        run_id: &str,
        driver: std::sync::Arc<dyn AgentDriver>,
        checkpoint: IngressCheckpoint,
        sink: S,
        store: Arc<dyn IngressCheckpointStore>,
    ) -> Result<ResumeOutcome, String>
    where
        S: WorkerEventSink + Send + Sync + 'static,
    {
        let (prepared, start) = match checkpoint {
            IngressCheckpoint::NotFileIngress => return Ok(ResumeOutcome::NotFileIngress),
            // A prior discovery verdict is deliberately not consulted: a
            // fresh engine gets a fresh attempt, and a rollout that appeared
            // after the old engine went down is exactly what it will find.
            IngressCheckpoint::Armed { ingress, baseline, .. } => {
                let prepared = PreparedSource::with_baseline(ingress, baseline.into_iter().collect())?;
                (prepared, IngressStart::Discover)
            }
            IngressCheckpoint::Attached {
                ingress,
                path,
                session_id,
                consumed_bytes,
                identity,
                session_state,
            } => {
                let prepared = PreparedSource::with_baseline(ingress, HashSet::new())?;
                let mut candidate = validate_candidate(&prepared, &path)?
                    .ok_or_else(|| format!("recorded rollout {} is no longer attachable", path.display()))?;
                if candidate.session_id != session_id {
                    return Err(format!(
                        "recorded rollout {} now reports session {} rather than {session_id}",
                        path.display(),
                        candidate.session_id,
                    ));
                }
                if candidate.identity != identity {
                    // The pathname survived but the file behind it did not:
                    // rotated or replaced while this engine was down. The
                    // recorded offset indexes the dead incarnation, and the
                    // size check below cannot see that — a replacement that
                    // is merely long enough passes it, and the tail would
                    // then skip the new file's first `consumed_bytes` bytes
                    // and very likely resume mid-line.
                    return Err(format!(
                        "recorded rollout {} is a different file now ({:?} rather than {:?})",
                        path.display(),
                        candidate.identity,
                        identity,
                    ));
                }
                let size = candidate
                    .file
                    .metadata()
                    .map_err(|err| format!("metadata {}: {err}", path.display()))?
                    .len();
                if size < consumed_bytes {
                    // Shorter than what we already published means the file
                    // was truncated and regrown under the same name and
                    // inode. There is no offset in it that means what the
                    // checkpoint meant, so there is nothing honest to resume
                    // from.
                    return Err(format!(
                        "recorded rollout {} is {size} bytes but {consumed_bytes} were already consumed",
                        path.display(),
                    ));
                }
                // The record's own invariant, checked rather than trusted:
                // `consumed_bytes` is always immediately past a newline. A
                // truncate-and-regrow that happened to land at exactly the
                // same length keeps the inode and passes the size check, and
                // attaching mid-line there would splice a fragment of the
                // dead incarnation onto the new one's next line.
                verify_record_boundary(&mut candidate.file, &path, consumed_bytes)?;
                // Prove the driver can take its own recorded state back
                // before anything is attached. The reader would otherwise
                // discover this mid-flight, in a spawned task with nobody to
                // report to — and a rejected state there means the run reads
                // nothing at all, which is precisely the condition an
                // operator has to hear about rather than infer from silence.
                if let Some(state) = session_state.as_ref() {
                    let mut probe = driver
                        .progress_session(&ProgressSessionConfig {
                            run_id: Some(run_id.to_owned()),
                            source: ProgressStreamSource::AgentJsonlFile,
                            ..ProgressSessionConfig::default()
                        })
                        .ok_or_else(|| "driver produced no progress session to resume".to_owned())?;
                    probe.restore_resume_state(state)?;
                }
                (
                    prepared,
                    IngressStart::Resume {
                        candidate,
                        file_offset: consumed_bytes,
                        session_state,
                    },
                )
            }
        };
        self.spawn_ingress(run_id, driver, prepared, sink, store, start)?;
        // Readoption has no second act: the pane is already live, so there is
        // no later spawn acknowledgement to wait for. Activating here is what
        // makes the sequence `re-establish → tail → turn boundary` and not
        // `re-establish → wait forever`.
        self.activate_run(run_id);
        Ok(ResumeOutcome::Reestablished)
    }

    fn spawn_ingress<S>(
        &self,
        run_id: &str,
        driver: std::sync::Arc<dyn AgentDriver>,
        prepared: PreparedSource,
        sink: S,
        store: Arc<dyn IngressCheckpointStore>,
        start: IngressStart,
    ) -> Result<(), String>
    where
        S: WorkerEventSink + Send + Sync + 'static,
    {
        let mut runs = self
            .runs
            .lock()
            .map_err(|_| "agent JSONL manager mutex poisoned".to_owned())?;
        if runs.contains_key(run_id) {
            tracing::warn!(run_id, "agent JSONL progress: duplicate prepare ignored");
            return Ok(());
        }

        let (activate_tx, activate_rx) = oneshot::channel();
        let (halt_tx, halt_rx) = watch::channel(StreamHalt::Running);
        let task_run_id = run_id.to_owned();
        let overdue_after = self.discovery_overdue_after;
        tokio::spawn(async move {
            run_prepared(
                task_run_id,
                RunPreparedConfig {
                    driver,
                    prepared,
                    sink,
                    store,
                    overdue_after,
                },
                start,
                activate_rx,
                halt_rx,
            )
            .await;
        });
        runs.insert(
            run_id.to_owned(),
            RunHandle {
                activate: Some(activate_tx),
                halt: halt_tx,
            },
        );
        Ok(())
    }

    /// Let a prepared ingress discover and dispatch now that the live slot is
    /// registered. Idempotent for repeated spawn acknowledgements.
    pub fn activate_run(&self, run_id: &str) {
        let Ok(mut runs) = self.runs.lock() else {
            tracing::warn!(run_id, "agent JSONL progress: manager mutex poisoned during activate");
            return;
        };
        let Some(handle) = runs.get_mut(run_id) else {
            return;
        };
        if let Some(activate) = handle.activate.take() {
            let _ = activate.send(());
        }
    }

    /// Close the source stream. The shared JSONL reader then flushes any
    /// unterminated final fragment and drains its ordered dispatch queue.
    ///
    /// Teardown, not completion: bytes the tail had not read yet are dropped.
    pub fn stop_run(&self, run_id: &str) {
        let Ok(mut runs) = self.runs.lock() else {
            tracing::warn!(run_id, "agent JSONL progress: manager mutex poisoned during stop");
            return;
        };
        if let Some(handle) = runs.remove(run_id) {
            let _ = handle.halt.send(StreamHalt::Cancel);
            drop(handle.activate);
        }
    }
}

struct RunPreparedConfig<S> {
    driver: Arc<dyn AgentDriver>,
    prepared: PreparedSource,
    sink: S,
    store: Arc<dyn IngressCheckpointStore>,
    overdue_after: Duration,
}

async fn run_prepared<S>(
    run_id: String,
    config: RunPreparedConfig<S>,
    start: IngressStart,
    mut activate: oneshot::Receiver<()>,
    mut halt: watch::Receiver<StreamHalt>,
) where
    S: WorkerEventSink + Send + Sync + 'static,
{
    let RunPreparedConfig {
        driver,
        prepared,
        sink,
        store,
        overdue_after,
    } = config;
    tokio::select! {
        result = &mut activate => {
            if result.is_err() {
                return;
            }
        }
        changed = halt.changed() => {
            let _ = changed;
            return;
        }
    }

    let (candidate, start_offset, session_state, discovery_secs) = match start {
        IngressStart::Discover => {
            let discovery = Discovery {
                run_id: &run_id,
                prepared: &prepared,
                sink: &sink,
                store: &store,
                overdue_after,
            };
            let (candidate, discovery_secs) = match discovery.run(&mut halt).await {
                Ok(Some(found)) => found,

                Ok(None) => return,
                Err(err) => {
                    // This ends the ingress for the run: no bytes will ever
                    // be read, so no progress event and no driver-start
                    // signal will ever come from this path. Whether the
                    // rollout exists is a separate question the reaper asks
                    // the filesystem directly (`transcript_liveness`); the
                    // message here says what discovery actually saw so the
                    // two are never confused again.
                    tracing::error!(
                        run_id,
                        %err,
                        "agent JSONL progress: discovery failed; this run's rollout will not be tailed",
                    );
                    return;
                }
            };
            // Promote the run's checkpoint from `Armed` to `Attached` the
            // moment the rollout is identified, before a single byte is read.
            // A restart in the window between attaching and the first event
            // then resumes at offset 0 of the right file, rather than
            // re-running discovery against a baseline the file now defeats.
            let attached = IngressCheckpoint::Attached {
                ingress: prepared.ingress.clone(),
                path: candidate.path.clone(),
                session_id: candidate.session_id.clone(),
                consumed_bytes: 0,
                identity: candidate.identity,
                session_state: None,
            };
            if let Err(err) = store.store_ingress_checkpoint(&run_id, &attached) {
                tracing::warn!(
                    run_id,
                    %err,
                    "agent JSONL progress: could not record the attached rollout",
                );
            }
            (candidate, 0, None, Some(discovery_secs))
        }
        IngressStart::Resume {
            candidate,
            file_offset,
            session_state,
        } => (candidate, file_offset, session_state, None),
    };
    tracing::info!(
        run_id,
        session_id = %candidate.session_id,
        path = %candidate.path.display(),
        start_offset,
        discovery_secs,
        "agent JSONL progress: attached rollout",
    );
    sink.record_ingress_observation(
        &run_id,
        IngressObservation::Attached {
            path: candidate.path.clone(),
            session_id: candidate.session_id.clone(),
            discovery_secs,
        },
    )
    .await;
    // Proof of life at attach, in case discovery never observed size/mtime
    // growth (readoption of an already-complete rollout, or a file that
    // appeared fully formed between polls). Discovery records the same
    // signal earlier for a growing-but-unparseable first line; the sink is
    // idempotent, so a second call here keeps the first timestamp.

    sink.record_driver_attach(&run_id);
    let transcript_path = candidate.path.clone();
    // Resolved here, once, so the per-event write below is a single keyed
    // update. A store that cannot resolve it still gets its checkpoints — the
    // run-id keyed path is the fallback, and losing the durable resume point
    // over a failed lookup would be a far worse trade than a slower write.
    let target = match store.resolve_checkpoint_target(&run_id) {
        Ok(target) => target,
        Err(err) => {
            tracing::warn!(
                run_id,
                %err,
                "agent JSONL progress: could not pre-resolve the checkpoint destination; \
                 falling back to resolving it on each write",
            );
            CheckpointTarget::ByRunId(run_id.clone())
        }
    };
    let checkpointer = AttachedCheckpointer::builder()
        .run_id(run_id.clone())
        .store(store)
        .target(target)
        .ingress(prepared.ingress.clone())
        .path(candidate.path.clone())
        .session_id(candidate.session_id.clone())
        .map(Arc::new(Mutex::new(StreamFileMap::default())))
        .build();

    let (reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
    let tail_halt = halt.clone();
    let tail_source = prepared.clone();
    let tail_map = checkpointer.map.clone();
    let tail = tokio::spawn(async move {
        if let Err(err) = stream_file_bytes(tail_source, candidate, start_offset, tail_map, writer, tail_halt).await {
            tracing::warn!(%err, "agent JSONL progress: file tail ended with error");
        }
    });

    // Codex and Grok link their `sessions/` directory into Boss's
    // per-execution transcript store. Persist the resolved target, not the
    // temporary per-run-home pathname, so terminal readers keep working after
    // the home is reaped.
    let transcript_path = std::fs::canonicalize(&transcript_path).unwrap_or(transcript_path);
    let config = ProgressSessionConfig {
        run_id: Some(run_id.clone()),
        identity_store: sink.progress_identity_store(),
        source: ProgressStreamSource::AgentJsonlFile,
        transcript_path: Some(transcript_path),
        resume_state: session_state,
    };
    let driver_slug = driver.descriptor().name;
    let stats = crate::stdout_progress::run_jsonl_progress_ingress_checkpointed(
        &run_id,
        driver_slug,
        driver,
        reader,
        &sink,
        config,
        Some(&checkpointer),
    )
    .await;
    if stats.resume_state_rejected {
        tracing::error!(
            run_id,
            "agent JSONL progress: the driver refused the recorded session state, so this run's \
             rollout is not being read. Its turns will not produce boundaries until it is restarted.",
        );
    }
    tail.abort();
    let _ = tail.await;
}

/// Preserve the durable checkpoint schema while retaining detailed reasons
/// in discovery logs and the checkpoint's narrative.
fn checkpoint_rejection(reason: &crate::agent_jsonl_discovery::CandidateRejection) -> CandidateRejectReason {
    use crate::agent_jsonl_discovery::CandidateRejection as R;
    match reason {
        R::InBaseline | R::NotSingleLinkRegularFile => CandidateRejectReason::UnsafeFile,
        R::OutsideRoot => CandidateRejectReason::OutsideRoot,
        R::IdentityChanged => CandidateRejectReason::IdentityChanged,
        R::Empty | R::SessionMetaUnterminated { .. } => CandidateRejectReason::IncompleteSessionMeta,
        R::SessionMetaOversized { .. } => CandidateRejectReason::OversizedSessionMeta,
        R::NotSessionMeta { .. } | R::MissingPayload | R::MissingSessionId | R::MissingCwd => {
            CandidateRejectReason::InvalidSessionMeta
        }
        R::SessionIdMismatch { .. } => CandidateRejectReason::SessionIdMismatch,
        R::CwdNotResolvable { .. } | R::CwdMismatch { .. } => CandidateRejectReason::WorkspaceMismatch,
        R::NameMismatch { .. } => CandidateRejectReason::FilenameMismatch,
        R::Unreadable(_) => CandidateRejectReason::IoOrParseError,
    }
}

/// One run's discovery: the poll loop that waits for exactly one new,
/// workspace-correlated rollout to appear under the prepared root.
struct Discovery<'a, S> {
    run_id: &'a str,
    prepared: &'a PreparedSource,
    sink: &'a S,
    store: &'a Arc<dyn IngressCheckpointStore>,
    overdue_after: Duration,
}

impl<S> Discovery<'_, S>
where
    S: WorkerEventSink,
{
    /// Poll until the rollout appears, the ingress is halted, or discovery
    /// fails outright. Returns the candidate and how many seconds it took.
    ///
    /// There is no deadline. Past `overdue_after` the run is reported as
    /// overdue — durably on its checkpoint and as an observation on
    /// its timeline — and polling continues, because the alternative was
    /// measured: a fixed give-up point shorter than the driver-start grace
    /// left live workers permanently unobserved and reaped as never-started
    /// (see [`DISCOVERY_OVERDUE_AFTER`]). What ends an unattached discovery
    /// is the run's own teardown ([`StreamHalt::Cancel`]) or a failure that
    /// polling cannot cure: the root changing identity, a failed scan task, or two
    /// new rollouts both claiming this one run.
    async fn run(&self, halt: &mut watch::Receiver<StreamHalt>) -> Result<Option<(Candidate, u64)>, String> {
        let started = tokio::time::Instant::now();
        let mut recorded_diagnostics = None;
        let mut recorded_live = false;
        let result = crate::agent_jsonl_discovery::discover_candidate_observed(
            self.prepared,
            halt,
            self.overdue_after,
            |rejected, reason, waited_secs, overdue, file_progress| {
                if file_progress && !recorded_live {
                    self.sink.record_driver_attach(self.run_id);
                    recorded_live = true;
                }
                let mut rejected: Vec<_> = rejected
                    .into_iter()
                    .filter(|(_, reason)| {
                        !matches!(reason, crate::agent_jsonl_discovery::CandidateRejection::InBaseline)
                    })
                    .collect();
                rejected.sort_by(|a, b| a.0.cmp(&b.0));
                let rejected_candidates = rejected.len();
                let rejections: Vec<_> = rejected
                    .into_iter()
                    .take(4)
                    .map(|(path, reason)| CandidateRejection {
                        file_name: path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .chars()
                            .take(256)
                            .collect(),
                        reason: checkpoint_rejection(&reason),
                    })
                    .collect();
                let diagnostics = (overdue, rejected_candidates, rejections.clone());
                let changed = recorded_diagnostics.as_ref() != Some(&diagnostics);
                recorded_diagnostics = Some(diagnostics);
                async move {
                    if overdue && changed {
                        self.record_overdue(waited_secs, rejected_candidates, &rejections, reason)
                            .await;
                    }
                    false
                }
            },
        )
        .await;
        match result {
            Ok(candidate) => Ok(candidate.map(|candidate| (candidate, started.elapsed().as_secs()))),
            Err(err) => {
                let (_, count, rejections) = recorded_diagnostics.unwrap_or_default();
                self.record_failure(started.elapsed().as_secs(), count, &rejections, &err)
                    .await;
                Err(err)
            }
        }
    }

    async fn record_overdue(
        &self,
        waited_secs: u64,
        rejected_candidates: usize,
        rejections: &[CandidateRejection],
        reason: String,
    ) {
        let root = self.prepared.root.path.clone();
        tracing::warn!(
            run_id = self.run_id,
            root = %root.display(),
            waited_secs,
            rejected_candidates,
            "agent JSONL progress: discovery overdue — the driver has not written its rollout yet (or \
             wrote one that does not correlate to this run). Still polling; a driver-start reap of this \
             run is a driver that was never observed, not one that never started",
        );
        self.store_record(
            DiscoveryVerdict::Overdue,
            waited_secs,
            rejected_candidates,
            rejections,
            format!("{reason}; still looking"),
        );
        self.sink
            .record_ingress_observation(
                self.run_id,
                IngressObservation::DiscoveryOverdue {
                    root,
                    waited_secs,
                    rejected_candidates,
                },
            )
            .await;
    }

    async fn record_failure(
        &self,
        waited_secs: u64,
        rejected_candidates: usize,
        rejections: &[CandidateRejection],
        reason: &str,
    ) {
        let root = self.prepared.root.path.clone();
        self.store_record(
            DiscoveryVerdict::Failed,
            waited_secs,
            rejected_candidates,
            rejections,
            reason.to_owned(),
        );
        self.sink
            .record_ingress_observation(
                self.run_id,
                IngressObservation::DiscoveryFailed {
                    root,
                    waited_secs,
                    rejected_candidates,
                    reason: reason.to_owned(),
                },
            )
            .await;
    }

    /// Re-write the run's `Armed` checkpoint carrying `verdict`. The
    /// baseline is preserved so a re-armed discovery after an engine restart
    /// still diffs against the pre-spawn snapshot.
    fn store_record(
        &self,
        verdict: DiscoveryVerdict,
        waited_secs: u64,
        rejected_candidates: usize,
        rejections: &[CandidateRejection],
        reason: String,
    ) {
        let checkpoint = IngressCheckpoint::Armed {
            ingress: self.prepared.ingress.clone(),
            baseline: self.prepared.baseline.iter().cloned().collect(),
            discovery: Some(DiscoveryRecord {
                verdict,
                at_epoch_secs: boss_engine_utils::epoch_time::now_epoch_secs(),
                waited_secs,
                rejected_candidates,
                rejections: rejections.to_vec(),
                reason,
            }),
        };
        if let Err(err) = self.store.store_ingress_checkpoint(self.run_id, &checkpoint) {
            tracing::warn!(
                run_id = self.run_id,
                %err,
                "agent JSONL progress: could not record the discovery verdict on the run's checkpoint",
            );
        }
    }
}

fn validate_descriptor_before_publish(
    prepared: &PreparedSource,
    path: &Path,
    file: &std::fs::File,
    expected_identity: FileIdentity,
) -> Result<(), String> {
    prepared.root.revalidate()?;
    let opened = file
        .metadata()
        .map_err(|err| format!("metadata {}: {err}", path.display()))?;
    if !opened.is_file() || file_identity(&opened) != expected_identity {
        return Err(format!(
            "validated rollout descriptor {} changed identity",
            path.display()
        ));
    }
    if descriptor_is_unlinked(&opened) {
        // Re-check the descriptor after the root check. An unlinked fd has no
        // surviving alias and therefore cannot be mutated through another
        // pathname between the read and publication.
        let after = file
            .metadata()
            .map_err(|err| format!("metadata {}: {err}", path.display()))?;
        if descriptor_is_unlinked(&after) && file_identity(&after) == expected_identity {
            return Ok(());
        }
    } else if named_descriptor_matches(prepared, path, file, expected_identity)? {
        return Ok(());
    }
    Err(format!(
        "validated rollout descriptor {} is no longer exclusively named by the tracked path",
        path.display()
    ))
}

fn revalidate_truncated_stream(
    prepared: &PreparedSource,
    path: &Path,
    expected_session_id: &str,
    file: &mut std::fs::File,
    expected_identity: FileIdentity,
) -> Result<(), String> {
    if !named_descriptor_matches(prepared, path, file, expected_identity)? {
        return Err(format!(
            "truncated rollout {} lost descriptor/path identity",
            path.display()
        ));
    }
    if validated_session_meta(prepared, path, file, Some(expected_session_id))?.is_none() {
        return Err(format!("truncated rollout {} lost run correlation", path.display()));
    }
    if !named_descriptor_matches(prepared, path, file, expected_identity)? {
        return Err(format!(
            "truncated rollout {} changed descriptor/path identity during validation",
            path.display()
        ));
    }
    Ok(())
}

async fn stream_file_bytes(
    prepared: PreparedSource,
    candidate: Candidate,
    start_offset: u64,
    map: Arc<Mutex<StreamFileMap>>,
    writer: tokio::io::DuplexStream,
    halt: watch::Receiver<StreamHalt>,
) -> Result<(), String> {
    stream_file_bytes_with_test_hooks(
        prepared,
        candidate,
        start_offset,
        map,
        writer,
        halt,
        |_| Ok(()),
        |_| Ok(()),
    )
    .await
}

/// Stream only descriptors returned by [`validate_candidate`].
///
/// The hooks are deterministic adversarial-test seams immediately before a
/// descriptor read and between validating and installing a rotation.
/// Production passes no-ops. They do not change the validation path exercised
/// by tests.
#[allow(clippy::too_many_arguments)]
async fn stream_file_bytes_with_test_hooks<F, G>(
    prepared: PreparedSource,
    candidate: Candidate,
    start_offset: u64,
    map: Arc<Mutex<StreamFileMap>>,
    mut writer: tokio::io::DuplexStream,
    mut halt: watch::Receiver<StreamHalt>,
    mut before_descriptor_read: G,
    mut after_rotation_validation: F,
) -> Result<(), String>
where
    F: FnMut(&Candidate) -> Result<(), String> + Send,
    G: FnMut(&Path) -> Result<(), String> + Send,
{
    let Candidate {
        path,
        session_id,
        mut file,
        identity: mut opened_identity,
    } = candidate;
    let mut offset = start_offset;
    let mut buffer = vec![0u8; FILE_CHUNK_BYTES];

    loop {
        if *halt.borrow() == StreamHalt::Cancel {
            return Ok(());
        }
        prepared.root.revalidate()?;

        // Drain the descriptor that was securely opened and correlated by
        // validate_candidate before consulting the pathname again. A rename
        // after validation cannot substitute bytes from a second file.
        validate_descriptor_before_publish(&prepared, &path, &file, opened_identity)?;
        before_descriptor_read(&path)?;
        let opened_metadata = file
            .metadata()
            .map_err(|err| format!("metadata {}: {err}", path.display()))?;
        let size = opened_metadata.len();
        if size < offset {
            // Revalidate the restarted logical stream on this exact
            // descriptor before publishing either its prefix or a delimiter.
            revalidate_truncated_stream(&prepared, &path, &session_id, &mut file, opened_identity)?;
            // A truncation keeps the inode, so the new incarnation of the
            // bytes is still `opened_identity` — what restarts is the offset.
            record_delimiter(&map, 0, opened_identity)?;
            write_or_halt(&mut writer, b"\n", &mut halt).await?;
            offset = 0;
            continue;
        }
        if size > offset {
            file.seek(SeekFrom::Start(offset))
                .map_err(|err| format!("seek {}: {err}", path.display()))?;
            let read_len = usize::try_from((size - offset).min(FILE_CHUNK_BYTES as u64))
                .map_err(|_| "rollout read size overflow".to_owned())?;
            let count = file
                .read(&mut buffer[..read_len])
                .map_err(|err| format!("read {}: {err}", path.display()))?;
            if count > 0 {
                // Re-read correlation from the same descriptor and then
                // re-check descriptor/path identity after the byte read.
                // Nothing from `buffer` is published before both checks.
                if validated_session_meta(&prepared, &path, &mut file, Some(&session_id))?.is_none() {
                    return Err(format!(
                        "rollout {} lost run correlation before publication",
                        path.display()
                    ));
                }
                validate_descriptor_before_publish(&prepared, &path, &file, opened_identity)?;
                // Record the correspondence before the bytes are visible to
                // the reader: the checkpointer resolves the reader's position
                // through this map, and a position it cannot resolve leaves
                // the durable resume point where it was.
                record_bytes(&map, offset, count as u64, opened_identity)?;
                offset += count as u64;
                write_or_halt(&mut writer, &buffer[..count], &mut halt).await?;
                continue;
            }
        }

        let path_metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tokio::select! {
                    _ = tokio::time::sleep(FILE_POLL) => continue,
                    changed = halt.changed() => {
                        // Every sender dropped: nothing can ever halt us. End
                        // cleanly rather than spinning the poll loop forever.
                        // A prior `Cancel` still reaches us via `changed()`'s
                        // last Ok (version bumps before close).
                        if changed.is_err() {
                            return Ok(());
                        }
                        continue;
                    }
                }
            }
            Err(err) => return Err(format!("stat {}: {err}", path.display())),
        };
        if path_metadata.file_type().is_symlink() || !single_link_regular(&path_metadata) {
            return Err(format!(
                "rollout {} is no longer a real single-link file",
                path.display()
            ));
        }
        let path_identity = file_identity(&path_metadata);
        if path_identity != opened_identity {
            // Same-path rotation/replacement. Revalidate the new file's
            // session_meta before accepting it, and force a line boundary so
            // an old incomplete fragment cannot join the new first line.
            let rotated = validate_candidate(&prepared, &path)?
                .filter(|new| new.session_id == session_id)
                .ok_or_else(|| format!("rotated rollout {} lost run correlation", path.display()))?;
            after_rotation_validation(&rotated)?;
            // Everything from the delimiter on belongs to the new inode; the
            // segments before it keep naming the old one, so a checkpoint
            // taken while the reader is still behind the boundary stays
            // paired with the incarnation its offset came from.
            record_delimiter(&map, 0, rotated.identity)?;
            write_or_halt(&mut writer, b"\n", &mut halt).await?;
            // Transfer the exact descriptor whose session/cwd/filename/link
            // and inode evidence was just validated. Never reopen `path`.
            file = rotated.file;
            opened_identity = rotated.identity;
            offset = 0;
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(FILE_POLL) => {}
            changed = halt.changed() => {
                // Re-run the loop on any change; the top-of-loop check
                // handles `Cancel`. Every sender dropped is terminal: end
                // cleanly rather than busy-looping the poll.
                if changed.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Check the invariant [`IngressCheckpoint::Attached::consumed_bytes`]
/// asserts about itself: it is immediately past a newline.
///
/// Offset zero is trivially a boundary. Anything else must have `\n` as its
/// preceding byte, or the offset does not name the start of a record in *this*
/// file — which means attaching there would hand the reader a fragment.
fn verify_record_boundary(file: &mut fs::File, path: &Path, consumed_bytes: u64) -> Result<(), String> {
    if consumed_bytes == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(consumed_bytes - 1))
        .map_err(|err| format!("seek {}: {err}", path.display()))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)
        .map_err(|err| format!("read {} at {}: {err}", path.display(), consumed_bytes - 1))?;
    if last[0] != b'\n' {
        return Err(format!(
            "recorded rollout {} does not end a record at {consumed_bytes} (preceding byte is {:?}, not a newline)",
            path.display(),
            last[0] as char,
        ));
    }
    Ok(())
}

fn record_bytes(map: &Mutex<StreamFileMap>, file_start: u64, len: u64, identity: FileIdentity) -> Result<(), String> {
    map.lock()
        .map_err(|_| "rollout offset map poisoned".to_owned())?
        .record_bytes(file_start, len, identity);
    Ok(())
}

fn record_delimiter(map: &Mutex<StreamFileMap>, next_file_start: u64, identity: FileIdentity) -> Result<(), String> {
    map.lock()
        .map_err(|_| "rollout offset map poisoned".to_owned())?
        .record_delimiter(next_file_start, identity);
    Ok(())
}

/// Publish `bytes` to the reader, abandoning the write only on
/// [`StreamHalt::Cancel`].
async fn write_or_halt(
    writer: &mut tokio::io::DuplexStream,
    bytes: &[u8],
    halt: &mut watch::Receiver<StreamHalt>,
) -> Result<(), String> {
    if *halt.borrow() == StreamHalt::Cancel {
        return Ok(());
    }
    tokio::select! {
        result = writer.write_all(bytes) => result.map_err(|err| format!("write JSONL stream: {err}")),
        () = wait_for_cancel(halt) => Ok(()),
    }
}

/// Resolve only once the halt state reaches [`StreamHalt::Cancel`].
async fn wait_for_cancel(halt: &mut watch::Receiver<StreamHalt>) {
    loop {
        if *halt.borrow() == StreamHalt::Cancel {
            return;
        }
        if halt.changed().await.is_err() {
            // Every sender dropped: nothing can ever cancel now. Park forever
            // so the sibling write branch decides the select.
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
#[path = "agent_jsonl_progress_tests/discovery.rs"]
mod discovery_tests;

#[cfg(test)]
#[path = "agent_jsonl_progress_tests/stream.rs"]
mod tests;
