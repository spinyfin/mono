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
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, watch};

use crate::driver::{AgentDriver, AgentJsonlFileIngress, ProgressSessionConfig, ProgressStreamSource};
use crate::stdout_progress::{ProgressCheckpointSink, WorkerEventSink};

const DISCOVERY_POLL: Duration = Duration::from_millis(100);
const FILE_POLL: Duration = Duration::from_millis(50);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DISCOVERY_DIRS: usize = 512;
const MAX_DISCOVERY_MATCHES: usize = 8;
const MAX_SESSION_META_BYTES: u64 = 64 * 1024;
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

/// Which incarnation of a pathname a descriptor or an offset refers to.
///
/// Public because [`IngressCheckpoint::Attached`] persists it: a resume point
/// is only meaningful paired with the incarnation it was measured against.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity { device: 0, inode: 0 }
}

#[cfg(unix)]
fn single_link_regular(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file() && metadata.nlink() == 1
}

#[cfg(not(unix))]
fn single_link_regular(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
}

#[cfg(unix)]
fn descriptor_is_unlinked(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file() && metadata.nlink() == 0
}

#[cfg(not(unix))]
fn descriptor_is_unlinked(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[derive(Clone, Debug)]
struct VerifiedRoot {
    path: PathBuf,
    canonical: PathBuf,
    identity: FileIdentity,
}

impl VerifiedRoot {
    fn new(path: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(path).map_err(|err| format!("stat {}: {err}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!("{} is not a real directory", path.display()));
        }
        let canonical = fs::canonicalize(path).map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            canonical,
            identity: file_identity(&metadata),
        })
    }

    fn revalidate(&self) -> Result<(), String> {
        let metadata =
            fs::symlink_metadata(&self.path).map_err(|err| format!("stat {}: {err}", self.path.display()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || file_identity(&metadata) != self.identity
            || fs::canonicalize(&self.path).ok().as_ref() != Some(&self.canonical)
        {
            return Err(format!("JSONL root {} changed identity", self.path.display()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct PreparedSource {
    ingress: AgentJsonlFileIngress,
    root: VerifiedRoot,
    canonical_workspace: PathBuf,
    baseline: HashSet<PathBuf>,
}

impl PreparedSource {
    fn new(ingress: AgentJsonlFileIngress) -> Result<Self, String> {
        let root = VerifiedRoot::new(&ingress.directory)?;
        let baseline = scan_matching_paths(&root, &ingress)?;
        Self::with_baseline(ingress, baseline)
    }

    /// [`Self::new`] against a baseline that was captured earlier — the
    /// pre-spawn snapshot read back off the run's durable checkpoint. Taking a
    /// fresh snapshot on the readoption path would be worse than useless: the
    /// run's own rollout already exists by then, so it would be baselined away
    /// and discovery would wait out its timeout finding nothing.
    fn with_baseline(ingress: AgentJsonlFileIngress, baseline: HashSet<PathBuf>) -> Result<Self, String> {
        let root = VerifiedRoot::new(&ingress.directory)?;
        let canonical_workspace = fs::canonicalize(&ingress.workspace_path)
            .map_err(|err| format!("canonicalize workspace {}: {err}", ingress.workspace_path.display()))?;
        Ok(Self {
            ingress,
            root,
            canonical_workspace,
            baseline,
        })
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

/// How an in-flight file ingress is being brought to an end.
///
/// [`Self::Cancel`] is teardown: the engine is releasing the pane and unread
/// bytes are forfeit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum StreamHalt {
    /// Normal operation: keep tailing the growing file.
    #[default]
    Running,
    /// Tear down now. Anything unread is dropped.
    Cancel,
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
#[derive(Default)]
pub struct AgentJsonlProgressManager {
    runs: Mutex<HashMap<String, RunHandle>>,
}

impl AgentJsonlProgressManager {
    pub fn new() -> Self {
        Self::default()
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
            IngressCheckpoint::Armed { ingress, baseline } => {
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
        tokio::spawn(async move {
            run_prepared(task_run_id, driver, prepared, sink, store, start, activate_rx, halt_rx).await;
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

#[allow(clippy::too_many_arguments)]
async fn run_prepared<S>(
    run_id: String,
    driver: std::sync::Arc<dyn AgentDriver>,
    prepared: PreparedSource,
    sink: S,
    store: Arc<dyn IngressCheckpointStore>,
    start: IngressStart,
    mut activate: oneshot::Receiver<()>,
    mut halt: watch::Receiver<StreamHalt>,
) where
    S: WorkerEventSink + Send + Sync + 'static,
{
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

    let (candidate, start_offset, session_state) = match start {
        IngressStart::Discover => {
            let candidate = match discover_candidate(&prepared, &mut halt).await {
                Ok(Some(candidate)) => candidate,
                Ok(None) => return,
                Err(err) => {
                    tracing::warn!(run_id, %err, "agent JSONL progress: discovery failed");
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
            (candidate, 0, None)
        }
        IngressStart::Resume {
            candidate,
            file_offset,
            session_state,
        } => (candidate, file_offset, session_state),
    };
    tracing::info!(
        run_id,
        session_id = %candidate.session_id,
        path = %candidate.path.display(),
        start_offset,
        "agent JSONL progress: attached rollout",
    );
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

#[derive(Debug)]
struct Candidate {
    path: PathBuf,
    session_id: String,
    file: std::fs::File,
    identity: FileIdentity,
}

async fn discover_candidate(
    prepared: &PreparedSource,
    halt: &mut watch::Receiver<StreamHalt>,
) -> Result<Option<Candidate>, String> {
    let deadline = tokio::time::Instant::now() + DISCOVERY_TIMEOUT;
    loop {
        // A `Cancel` during discovery stops it: the engine is tearing the
        // ingress down and there is nothing left to attach to.
        if *halt.borrow() != StreamHalt::Running {
            return Ok(None);
        }
        prepared.root.revalidate()?;
        let paths = scan_matching_paths(&prepared.root, &prepared.ingress)?;
        let mut matches = paths
            .difference(&prepared.baseline)
            .filter_map(|path| validate_candidate(prepared, path).ok().flatten())
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.path.cmp(&right.path));
        match matches.len() {
            1 => return Ok(matches.pop()),
            count if count > 1 => {
                return Err(format!(
                    "{count} new rollout files matched one run; refusing ambiguous attachment"
                ));
            }
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "no correlated rollout appeared under {} within {}s",
                prepared.root.path.display(),
                DISCOVERY_TIMEOUT.as_secs()
            ));
        }
        tokio::select! {
            _ = tokio::time::sleep(DISCOVERY_POLL) => {}
            changed = halt.changed() => {
                let _ = changed;
                return Ok(None);
            }
        }
    }
}

fn scan_matching_paths(root: &VerifiedRoot, ingress: &AgentJsonlFileIngress) -> Result<HashSet<PathBuf>, String> {
    root.revalidate()?;
    let mut stack = vec![root.canonical.clone()];
    let mut visited_dirs = 0usize;
    let mut matches = HashSet::new();
    while let Some(dir) = stack.pop() {
        visited_dirs += 1;
        if visited_dirs > MAX_DISCOVERY_DIRS {
            return Err(format!(
                "rollout discovery exceeded {MAX_DISCOVERY_DIRS} directories under {}",
                root.path.display()
            ));
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("read {}: {err}", dir.display())),
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                let Ok(canonical) = fs::canonicalize(&path) else {
                    continue;
                };
                if canonical.starts_with(&root.canonical) {
                    stack.push(canonical);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(&ingress.filename_prefix) || !name.ends_with(&ingress.filename_suffix) {
                continue;
            }
            let Ok(canonical) = fs::canonicalize(&path) else {
                continue;
            };
            if canonical.starts_with(&root.canonical) {
                matches.insert(canonical);
                if matches.len() > MAX_DISCOVERY_MATCHES {
                    return Err(format!(
                        "rollout discovery exceeded {MAX_DISCOVERY_MATCHES} matching files under {}",
                        root.path.display()
                    ));
                }
            }
        }
    }
    Ok(matches)
}

fn validate_candidate(prepared: &PreparedSource, path: &Path) -> Result<Option<Candidate>, String> {
    prepared.root.revalidate()?;
    let metadata = fs::symlink_metadata(path).map_err(|err| format!("stat {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() || !single_link_regular(&metadata) {
        return Ok(None);
    }
    let canonical = fs::canonicalize(path).map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    if !canonical.starts_with(&prepared.root.canonical) {
        return Ok(None);
    }

    let mut file = open_no_follow(&canonical)?;
    let opened = file
        .metadata()
        .map_err(|err| format!("metadata {}: {err}", canonical.display()))?;
    let identity = file_identity(&opened);
    if !single_link_regular(&opened) || identity != file_identity(&metadata) {
        return Ok(None);
    }
    let Some(session_id) = validated_session_meta(prepared, &canonical, &mut file, None)? else {
        return Ok(None);
    };
    if !named_descriptor_matches(prepared, &canonical, &file, identity)? {
        return Ok(None);
    }
    Ok(Some(Candidate {
        path: canonical,
        session_id,
        file,
        identity,
    }))
}

fn open_no_follow(path: &Path) -> Result<std::fs::File, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|err| format!("open {}: {err}", path.display()))
}

fn tracked_path_identity(prepared: &PreparedSource, path: &Path) -> Result<Option<FileIdentity>, String> {
    prepared.root.revalidate()?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("stat {}: {err}", path.display())),
    };
    if metadata.file_type().is_symlink() || !single_link_regular(&metadata) {
        return Ok(None);
    }
    let canonical = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("canonicalize {}: {err}", path.display())),
    };
    if canonical != path || !canonical.starts_with(&prepared.root.canonical) {
        return Ok(None);
    }
    prepared.root.revalidate()?;
    Ok(Some(file_identity(&metadata)))
}

fn named_descriptor_matches(
    prepared: &PreparedSource,
    path: &Path,
    file: &std::fs::File,
    expected_identity: FileIdentity,
) -> Result<bool, String> {
    let opened_before = file
        .metadata()
        .map_err(|err| format!("metadata {}: {err}", path.display()))?;
    if !single_link_regular(&opened_before)
        || file_identity(&opened_before) != expected_identity
        || tracked_path_identity(prepared, path)? != Some(expected_identity)
    {
        return Ok(false);
    }

    // Re-read both sides so a path replacement during the first comparison
    // cannot make stale path metadata authorize publication.
    let opened_after = file
        .metadata()
        .map_err(|err| format!("metadata {}: {err}", path.display()))?;
    if !single_link_regular(&opened_after)
        || file_identity(&opened_after) != expected_identity
        || tracked_path_identity(prepared, path)? != Some(expected_identity)
    {
        return Ok(false);
    }
    Ok(true)
}

fn validated_session_meta(
    prepared: &PreparedSource,
    path: &Path,
    file: &mut std::fs::File,
    expected_session_id: Option<&str>,
) -> Result<Option<String>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|err| format!("seek session_meta {}: {err}", path.display()))?;
    let result = (|| {
        let mut first_line = Vec::new();
        let mut limited = std::io::BufReader::new((&mut *file).take(MAX_SESSION_META_BYTES));
        let bytes = limited
            .read_until(b'\n', &mut first_line)
            .map_err(|err| format!("read session_meta {}: {err}", path.display()))?;
        if bytes == 0 || first_line.last() != Some(&b'\n') {
            return Ok(None);
        }
        let record: serde_json::Value = serde_json::from_slice(&first_line)
            .map_err(|err| format!("parse session_meta {}: {err}", path.display()))?;
        if record.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
            return Ok(None);
        }
        let Some(payload) = record.get("payload").and_then(serde_json::Value::as_object) else {
            return Ok(None);
        };
        let Some(session_id) = payload.get("id").and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        if expected_session_id.is_some_and(|expected| expected != session_id) {
            return Ok(None);
        }
        let Some(cwd) = payload.get("cwd").and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        let Ok(canonical_cwd) = fs::canonicalize(cwd) else {
            return Ok(None);
        };
        if canonical_cwd != prepared.canonical_workspace {
            return Ok(None);
        }
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
        let expected_suffix = format!("-{session_id}{}", prepared.ingress.filename_suffix);
        if !name.ends_with(&expected_suffix) {
            return Ok(None);
        }
        Ok(Some(session_id.to_owned()))
    })();
    file.seek(SeekFrom::Start(0))
        .map_err(|err| format!("rewind validated rollout {}: {err}", path.display()))?;
    result
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
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use boss_protocol::{StopReason, WorkerEvent};
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use tokio::sync::Notify;

    use super::*;
    use crate::events_socket::IncomingHookEvent;

    #[derive(Clone, Default)]
    struct CaptureSink {
        events: Arc<Mutex<Vec<IncomingHookEvent>>>,
        notify: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl WorkerEventSink for CaptureSink {
        async fn dispatch_worker_event(&self, incoming: IncomingHookEvent) {
            self.events.lock().unwrap().push(incoming);
            self.notify.notify_waiters();
        }
    }

    /// In-memory stand-in for the `work_runs` column, so a test can read back
    /// the resume point the ingress actually wrote.
    #[derive(Clone, Default)]
    struct MemoryCheckpointStore {
        stored: Arc<Mutex<HashMap<String, IngressCheckpoint>>>,
    }

    impl MemoryCheckpointStore {
        fn get(&self, run_id: &str) -> Option<IngressCheckpoint> {
            self.stored.lock().unwrap().get(run_id).cloned()
        }
    }

    impl IngressCheckpointStore for MemoryCheckpointStore {
        fn store_ingress_checkpoint(&self, run_id: &str, checkpoint: &IngressCheckpoint) -> Result<(), String> {
            self.stored
                .lock()
                .unwrap()
                .insert(run_id.to_owned(), checkpoint.clone());
            Ok(())
        }

        fn load_ingress_checkpoint(&self, run_id: &str) -> Result<Option<IngressCheckpoint>, String> {
            Ok(self.get(run_id))
        }
    }

    fn test_store() -> Arc<dyn IngressCheckpointStore> {
        Arc::new(MemoryCheckpointStore::default())
    }

    /// The device/inode a checkpoint would have recorded for `path` as it is
    /// on disk right now.
    fn identity_of(path: &Path) -> FileIdentity {
        file_identity(&fs::metadata(path).unwrap())
    }

    /// Replace `path` with a file that has a different inode.
    ///
    /// Unlink-and-recreate is not enough: linux-sandbox mounts `/tmp` as
    /// tmpfs, which reuses the freed inode, so the "different file" check
    /// would not see a rotation. Creating the replacement first (while the
    /// original inode is still live) and renaming over keeps the new inode.
    fn replace_with_new_inode(path: &Path, contents: impl AsRef<[u8]>) {
        let tmp = path.with_extension("replacement");
        fs::write(&tmp, contents).unwrap();
        fs::rename(&tmp, path).unwrap();
    }

    /// The tail as every pre-existing test drives it: from byte zero, with an
    /// offset map nobody reads back. Shadows the production name so those
    /// tests keep asserting on exactly the code path they always did.
    async fn stream_file_bytes(
        prepared: PreparedSource,
        candidate: Candidate,
        writer: tokio::io::DuplexStream,
        halt: watch::Receiver<StreamHalt>,
    ) -> Result<(), String> {
        super::stream_file_bytes(
            prepared,
            candidate,
            0,
            Arc::new(Mutex::new(StreamFileMap::default())),
            writer,
            halt,
        )
        .await
    }

    async fn stream_file_bytes_with_test_hooks<F, G>(
        prepared: PreparedSource,
        candidate: Candidate,
        writer: tokio::io::DuplexStream,
        halt: watch::Receiver<StreamHalt>,
        before_descriptor_read: G,
        after_rotation_validation: F,
    ) -> Result<(), String>
    where
        F: FnMut(&Candidate) -> Result<(), String> + Send,
        G: FnMut(&Path) -> Result<(), String> + Send,
    {
        super::stream_file_bytes_with_test_hooks(
            prepared,
            candidate,
            0,
            Arc::new(Mutex::new(StreamFileMap::default())),
            writer,
            halt,
            before_descriptor_read,
            after_rotation_validation,
        )
        .await
    }

    fn rollout_text(workspace: &Path, thread_id: &str) -> String {
        [
            serde_json::json!({
                "type":"session_meta",
                "payload":{"id":thread_id,"cwd":workspace}
            }),
            serde_json::json!({
                "type":"event_msg",
                "payload":{"type":"task_started","turn_id":"turn-live"}
            }),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "name":"exec_command",
                    "call_id":"call-live",
                    "arguments":r#"{"cmd":"printf live"}"#
                }
            }),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-live",
                    "output":"live\n"
                }
            }),
            serde_json::json!({
                "type":"event_msg",
                "payload":{
                    "type":"task_complete",
                    "turn_id":"turn-live",
                    "last_agent_message":"done"
                }
            }),
        ]
        .into_iter()
        .map(|record| serde_json::to_string(&record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n"
    }

    fn rollout_with_marker(workspace: &Path, thread_id: &str, marker: &str) -> String {
        let meta = serde_json::json!({
            "type":"session_meta",
            "payload":{"id":thread_id,"cwd":workspace}
        });
        let marker = serde_json::json!({"marker": marker});
        format!(
            "{}\n{}\n",
            serde_json::to_string(&meta).unwrap(),
            serde_json::to_string(&marker).unwrap()
        )
    }

    fn long_rollout_with_marker(workspace: &Path, thread_id: &str, marker: &str) -> String {
        let mut rollout = rollout_with_marker(workspace, thread_id, marker);
        rollout.push_str(
            &serde_json::to_string(&serde_json::json!({
                "padding": "x".repeat(2048)
            }))
            .unwrap(),
        );
        rollout.push('\n');
        rollout
    }

    struct TailFixture {
        _temp: TempDir,
        prepared: PreparedSource,
        path: PathBuf,
        workspace: PathBuf,
        wrong_workspace: PathBuf,
        candidate: Candidate,
    }

    fn tail_fixture(thread_id: &str) -> TailFixture {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        let wrong_workspace = temp.path().join("wrong-workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&wrong_workspace).unwrap();
        let prepared = PreparedSource::new(AgentJsonlFileIngress {
            directory: sessions.clone(),
            filename_prefix: "rollout-".into(),
            filename_suffix: ".jsonl".into(),
            workspace_path: workspace.clone(),
        })
        .unwrap();
        let path = sessions.join(format!("rollout-test-{thread_id}.jsonl"));
        fs::write(&path, long_rollout_with_marker(&workspace, thread_id, "initial-stream")).unwrap();
        let candidate = validate_candidate(&prepared, &path).unwrap().unwrap();
        TailFixture {
            _temp: temp,
            prepared,
            path,
            workspace,
            wrong_workspace,
            candidate,
        }
    }

    #[tokio::test]
    async fn prepared_rollout_uses_shared_reader_and_exact_run_correlation() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        let wrong_workspace = temp.path().join("other-workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&wrong_workspace).unwrap();

        // A same-workspace rollout that predates preparation belongs to an
        // earlier process and must remain excluded.
        fs::write(
            sessions.join("rollout-old-thread-old.jsonl"),
            rollout_text(&workspace, "thread-old"),
        )
        .unwrap();

        let manager = AgentJsonlProgressManager::new();
        let sink = CaptureSink::default();
        manager
            .prepare_run(
                "run-live",
                Arc::new(crate::driver::CodexDriver::default()),
                AgentJsonlFileIngress {
                    directory: sessions.clone(),
                    filename_prefix: "rollout-".into(),
                    filename_suffix: ".jsonl".into(),
                    workspace_path: workspace.clone(),
                },
                sink.clone(),
                test_store(),
            )
            .unwrap();

        // A new file in the exact run-private root but with the wrong cwd is
        // not enough correlation and must be ignored.
        fs::write(
            sessions.join("rollout-new-thread-wrong.jsonl"),
            rollout_text(&wrong_workspace, "thread-wrong"),
        )
        .unwrap();
        let accepted = sessions.join("rollout-new-thread-live.jsonl");
        fs::write(&accepted, rollout_text(&workspace, "thread-live")).unwrap();
        manager.activate_run("run-live");

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if sink.events.lock().unwrap().len() >= 6 {
                    break;
                }
                sink.notify.notified().await;
            }
        })
        .await
        .expect("rollout events should reach the shared fanout");

        let events = sink.events.lock().unwrap();
        let canonical_accepted = fs::canonicalize(&accepted).unwrap();
        // Six, not five: this synthetic rollout has no armed CODEX_HOME behind
        // its run id — no arming attestation, no guard trace — which is exactly
        // the condition `GUARDS_SILENT_MARKER` reports (see the guard-trace
        // notification asserted below). A real dispatch always arms and attests
        // in `write_permission_config`, so the marker there means the hooks
        // genuinely are not being enforced.
        assert_eq!(events.len(), 6);
        assert!(events.iter().all(|event| event.run_id.as_deref() == Some("run-live")));
        assert!(
            events
                .iter()
                .all(|event| event.transcript_path.as_deref() == Some(canonical_accepted.to_string_lossy().as_ref()))
        );
        assert!(matches!(
            &events[0].event,
            WorkerEvent::SessionStart { session_id, .. } if session_id == "thread-live"
        ));
        assert!(matches!(&events[1].event, WorkerEvent::UserPromptSubmit { .. }));
        assert!(matches!(&events[2].event, WorkerEvent::PreToolUse { .. }));
        assert!(matches!(&events[3].event, WorkerEvent::PostToolUse { .. }));
        assert!(matches!(
            &events[4].event,
            WorkerEvent::Notification { message, .. }
                if message.starts_with(crate::driver::codex::GUARDS_SILENT_MARKER)
        ));
        assert!(matches!(
            &events[5].event,
            WorkerEvent::Stop {
                stop_reason: StopReason::Completed,
                ..
            }
        ));
        drop(events);

        manager.stop_run("run-live");
    }

    async fn read_until_contains(reader: &mut tokio::io::DuplexStream, observed: &mut Vec<u8>, needle: &[u8]) {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut chunk = [0u8; 4096];
            while !observed.windows(needle.len()).any(|window| window == needle) {
                let count = reader.read(&mut chunk).await.unwrap();
                assert!(count > 0, "tail closed before expected bytes arrived");
                observed.extend_from_slice(&chunk[..count]);
            }
        })
        .await
        .expect("tail should expose appended file bytes");
    }

    #[tokio::test]
    async fn raw_tail_handles_truncation_rotation_and_incomplete_final_line() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let ingress = AgentJsonlFileIngress {
            directory: sessions.clone(),
            filename_prefix: "rollout-".into(),
            filename_suffix: ".jsonl".into(),
            workspace_path: workspace.clone(),
        };
        let prepared = PreparedSource::new(ingress).unwrap();
        let path = sessions.join("rollout-test-thread-tail.jsonl");
        let meta = serde_json::to_string(&serde_json::json!({
            "type":"session_meta",
            "payload":{"id":"thread-tail","cwd":workspace}
        }))
        .unwrap();
        let long_partial = format!("{meta}\n{{\"partial\":\"{}\"", "x".repeat(512));
        fs::write(&path, &long_partial).unwrap();
        let candidate = validate_candidate(&prepared, &path).unwrap().unwrap();

        let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
        let (cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
        let tail = tokio::spawn(stream_file_bytes(prepared.clone(), candidate, writer, cancel_rx));
        let mut observed = Vec::new();
        read_until_contains(&mut reader, &mut observed, b"partial").await;

        // Shorter in-place rewrite forces offset reset and a separating
        // newline before the new JSONL stream.
        let truncated = format!("{meta}\n{{\"phase\":\"truncated\"}}\n");
        fs::write(&path, truncated).unwrap();
        read_until_contains(&mut reader, &mut observed, b"truncated").await;

        // Atomic same-path replacement changes inode. The replacement keeps
        // the same direct session metadata, so rotation is accepted.
        let replacement = sessions.join("replacement.jsonl");
        let rotated = format!("{meta}\n{{\"phase\":\"rotated\"}}\n");
        fs::write(&replacement, rotated).unwrap();
        fs::rename(&replacement, &path).unwrap();
        read_until_contains(&mut reader, &mut observed, b"rotated").await;

        // Cancellation is the logical worker EOF. The raw tail closes without
        // inventing a newline; the generic reader owns final-fragment parsing.
        {
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            use std::io::Write;
            write!(file, "{{\"final\":true").unwrap();
        }
        read_until_contains(&mut reader, &mut observed, b"final").await;
        cancel_tx.send(StreamHalt::Cancel).unwrap();
        reader.read_to_end(&mut observed).await.unwrap();
        tail.await.unwrap().unwrap();
        assert!(observed.ends_with(b"{\"final\":true"));
    }

    #[tokio::test]
    async fn initial_path_replacement_streams_only_the_validated_descriptor() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        let wrong_workspace = temp.path().join("wrong-workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&wrong_workspace).unwrap();
        let prepared = PreparedSource::new(AgentJsonlFileIngress {
            directory: sessions.clone(),
            filename_prefix: "rollout-".into(),
            filename_suffix: ".jsonl".into(),
            workspace_path: workspace.clone(),
        })
        .unwrap();
        let path = sessions.join("rollout-test-thread-attach.jsonl");
        fs::write(
            &path,
            rollout_with_marker(&workspace, "thread-attach", "validated-initial"),
        )
        .unwrap();
        let candidate = validate_candidate(&prepared, &path).unwrap().unwrap();

        // Replace the pathname after validation but before tail attachment.
        // The replacement has the right filename/session but the wrong cwd,
        // so a path reopen would expose attacker bytes before correlation.
        let attacker = sessions.join("attacker-initial.jsonl");
        fs::write(
            &attacker,
            rollout_with_marker(&wrong_workspace, "thread-attach", "unvalidated-replacement"),
        )
        .unwrap();
        fs::rename(&attacker, &path).unwrap();

        let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
        let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
        let result = stream_file_bytes(prepared, candidate, writer, cancel_rx).await;
        let mut observed = Vec::new();
        reader.read_to_end(&mut observed).await.unwrap();

        assert!(result.unwrap_err().contains("lost run correlation"));
        let observed = String::from_utf8(observed).unwrap();
        assert!(observed.contains("validated-initial"));
        assert!(!observed.contains("unvalidated-replacement"));
    }

    #[tokio::test]
    async fn rotation_path_replacement_streams_only_the_validated_rotation_descriptor() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        let wrong_workspace = temp.path().join("wrong-workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&wrong_workspace).unwrap();
        let prepared = PreparedSource::new(AgentJsonlFileIngress {
            directory: sessions.clone(),
            filename_prefix: "rollout-".into(),
            filename_suffix: ".jsonl".into(),
            workspace_path: workspace.clone(),
        })
        .unwrap();
        let path = sessions.join("rollout-test-thread-rotate.jsonl");
        fs::write(
            &path,
            rollout_with_marker(&workspace, "thread-rotate", "validated-initial"),
        )
        .unwrap();
        let candidate = validate_candidate(&prepared, &path).unwrap().unwrap();

        // Install a valid rotation so the tailer validates and opens it.
        let valid_rotation = sessions.join("valid-rotation.jsonl");
        fs::write(
            &valid_rotation,
            rollout_with_marker(&workspace, "thread-rotate", "validated-rotation"),
        )
        .unwrap();
        fs::rename(&valid_rotation, &path).unwrap();

        // The hook atomically replaces the path after rotation validation
        // but before the validated descriptor is installed in the tailer.
        let attacker = sessions.join("attacker-rotation.jsonl");
        fs::write(
            &attacker,
            rollout_with_marker(&wrong_workspace, "thread-rotate", "unvalidated-rotation"),
        )
        .unwrap();
        let hook_ran = Arc::new(AtomicBool::new(false));
        let hook_ran_for_tail = hook_ran.clone();
        let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
        let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
        let result = stream_file_bytes_with_test_hooks(
            prepared,
            candidate,
            writer,
            cancel_rx,
            |_| Ok(()),
            move |validated_rotation| {
                assert_eq!(validated_rotation.session_id, "thread-rotate");
                fs::rename(&attacker, &validated_rotation.path)
                    .map_err(|err| format!("replace path after rotation validation: {err}"))?;
                hook_ran_for_tail.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        let mut observed = Vec::new();
        reader.read_to_end(&mut observed).await.unwrap();

        assert!(hook_ran.load(Ordering::SeqCst));
        assert!(result.unwrap_err().contains("lost run correlation"));
        let observed = String::from_utf8(observed).unwrap();
        assert!(observed.contains("validated-initial"));
        assert!(observed.contains("validated-rotation"));
        assert!(!observed.contains("unvalidated-rotation"));
    }

    #[tokio::test]
    async fn surviving_hard_link_alias_cannot_inject_bytes_before_publication() {
        let fixture = tail_fixture("thread-alias");
        let alias = fixture.path.with_file_name("rollout-alias.jsonl");
        let replacement = fixture.path.with_file_name("rollout-replacement.jsonl");
        fs::write(
            &replacement,
            rollout_with_marker(&fixture.workspace, "thread-alias", "tracked-replacement"),
        )
        .unwrap();
        let attack_ran = Arc::new(AtomicBool::new(false));
        let attack_ran_in_loop = attack_ran.clone();
        let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
        let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
        let result = stream_file_bytes_with_test_hooks(
            fixture.prepared,
            fixture.candidate,
            writer,
            cancel_rx,
            move |tracked_path| {
                assert!(!attack_ran_in_loop.swap(true, Ordering::SeqCst));
                fs::hard_link(tracked_path, &alias).map_err(|err| format!("create surviving rollout alias: {err}"))?;
                fs::rename(&replacement, tracked_path).map_err(|err| format!("replace tracked rollout path: {err}"))?;
                let mut attacker = fs::OpenOptions::new()
                    .append(true)
                    .open(&alias)
                    .map_err(|err| format!("open surviving rollout alias: {err}"))?;
                std::io::Write::write_all(&mut attacker, b"{\"marker\":\"alias-attacker\"}\n")
                    .map_err(|err| format!("append through surviving rollout alias: {err}"))?;
                Ok(())
            },
            |_| Ok(()),
        )
        .await;
        let mut observed = Vec::new();
        reader.read_to_end(&mut observed).await.unwrap();

        assert!(attack_ran.load(Ordering::SeqCst));
        assert!(result.unwrap_err().contains("no longer exclusively named"));
        assert!(
            observed.is_empty(),
            "no bytes read before the failed post-read check may emerge"
        );
    }

    #[tokio::test]
    async fn wrong_workspace_truncation_is_rejected_before_prefix_publication() {
        let fixture = tail_fixture("thread-truncate-cwd");
        let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
        let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
        let tail = tokio::spawn(stream_file_bytes(
            fixture.prepared,
            fixture.candidate,
            writer,
            cancel_rx,
        ));
        let mut observed = Vec::new();
        read_until_contains(&mut reader, &mut observed, b"initial-stream").await;

        fs::write(
            &fixture.path,
            rollout_with_marker(&fixture.wrong_workspace, "thread-truncate-cwd", "wrong-cwd-prefix"),
        )
        .unwrap();
        let result = tail.await.unwrap();
        reader.read_to_end(&mut observed).await.unwrap();

        assert!(result.unwrap_err().contains("lost run correlation"));
        let observed = String::from_utf8(observed).unwrap();
        assert!(!observed.contains("wrong-cwd-prefix"));
        assert!(!observed.contains(fixture.wrong_workspace.to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn wrong_session_truncation_is_rejected_before_prefix_publication() {
        let fixture = tail_fixture("thread-truncate-session");
        let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
        let (_cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
        let tail = tokio::spawn(stream_file_bytes(
            fixture.prepared,
            fixture.candidate,
            writer,
            cancel_rx,
        ));
        let mut observed = Vec::new();
        read_until_contains(&mut reader, &mut observed, b"initial-stream").await;

        fs::write(
            &fixture.path,
            rollout_with_marker(&fixture.workspace, "thread-attacker", "wrong-session-prefix"),
        )
        .unwrap();
        let result = tail.await.unwrap();
        reader.read_to_end(&mut observed).await.unwrap();

        assert!(result.unwrap_err().contains("lost run correlation"));
        let observed = String::from_utf8(observed).unwrap();
        assert!(!observed.contains("wrong-session-prefix"));
        assert!(!observed.contains("thread-attacker"));
    }

    #[tokio::test]
    async fn valid_same_session_truncation_restarts_after_revalidation() {
        let fixture = tail_fixture("thread-truncate-valid");
        let (mut reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
        let (cancel_tx, cancel_rx) = watch::channel(StreamHalt::Running);
        let tail = tokio::spawn(stream_file_bytes(
            fixture.prepared,
            fixture.candidate,
            writer,
            cancel_rx,
        ));
        let mut observed = Vec::new();
        read_until_contains(&mut reader, &mut observed, b"initial-stream").await;

        fs::write(
            &fixture.path,
            rollout_with_marker(&fixture.workspace, "thread-truncate-valid", "valid-truncated-stream"),
        )
        .unwrap();
        read_until_contains(&mut reader, &mut observed, b"valid-truncated-stream").await;
        cancel_tx.send(StreamHalt::Cancel).unwrap();
        reader.read_to_end(&mut observed).await.unwrap();
        tail.await.unwrap().unwrap();

        let observed = String::from_utf8(observed).unwrap();
        assert!(observed.contains("valid-truncated-stream"));
    }

    #[tokio::test]
    async fn duplicate_prepare_keeps_exactly_one_run_handle() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let ingress = AgentJsonlFileIngress {
            directory: sessions,
            filename_prefix: "rollout-".into(),
            filename_suffix: ".jsonl".into(),
            workspace_path: workspace,
        };
        let manager = AgentJsonlProgressManager::new();
        let sink = CaptureSink::default();
        for _ in 0..2 {
            manager
                .prepare_run(
                    "run-duplicate",
                    Arc::new(crate::driver::CodexDriver::default()),
                    ingress.clone(),
                    sink.clone(),
                    test_store(),
                )
                .unwrap();
        }
        assert_eq!(manager.runs.lock().unwrap().len(), 1);
        manager.stop_run("run-duplicate");
        assert!(manager.runs.lock().unwrap().is_empty());
    }

    /// A second turn on an already-attached rollout, appended after the
    /// engine went away. Carries no `session_meta` of its own — the thread id
    /// was announced once, at the head of the file — which is what makes the
    /// restored session state load-bearing rather than decorative.
    fn second_turn_text() -> String {
        [
            serde_json::json!({
                "type":"event_msg",
                "payload":{"type":"task_started","turn_id":"turn-two"}
            }),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "name":"exec_command",
                    "call_id":"call-two",
                    "arguments":r#"{"cmd":"printf two"}"#
                }
            }),
            serde_json::json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-two",
                    "output":"two\n"
                }
            }),
            serde_json::json!({
                "type":"event_msg",
                "payload":{
                    "type":"task_complete",
                    "turn_id":"turn-two",
                    "last_agent_message":"done two"
                }
            }),
        ]
        .into_iter()
        .map(|record| serde_json::to_string(&record).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n"
    }

    async fn wait_for_stop(sink: &CaptureSink, at_least: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if sink
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|event| matches!(event.event, WorkerEvent::Stop { .. }))
                    .count()
                    >= at_least
                {
                    break;
                }
                sink.notify.notified().await;
            }
        })
        .await
        .expect("a turn boundary should reach the fanout");
    }

    /// The whole point of the readoption path, end to end.
    ///
    /// One engine attaches, reads a turn to its boundary, and disappears. A
    /// second engine — a fresh manager and a fresh sink, sharing only the
    /// durable checkpoint — resumes the same live rollout, and a turn that
    /// completes afterwards produces exactly the events it would have
    /// produced without the restart: no second `SessionStart`, no replay of
    /// the first turn's tool calls, no second `Stop` for a turn that already
    /// ended.
    #[tokio::test]
    async fn a_turn_completing_after_a_restart_produces_its_own_events_and_only_those() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let ingress = AgentJsonlFileIngress {
            directory: sessions.clone(),
            filename_prefix: "rollout-".into(),
            filename_suffix: ".jsonl".into(),
            workspace_path: workspace.clone(),
        };
        let store: Arc<dyn IngressCheckpointStore> = Arc::new(MemoryCheckpointStore::default());

        // Engine one.
        let before = AgentJsonlProgressManager::new();
        let before_sink = CaptureSink::default();
        before
            .prepare_run(
                "run-restart",
                Arc::new(crate::driver::CodexDriver::default()),
                ingress.clone(),
                before_sink.clone(),
                store.clone(),
            )
            .unwrap();
        let path = sessions.join("rollout-new-thread-restart.jsonl");
        fs::write(&path, rollout_text(&workspace, "thread-restart")).unwrap();
        before.activate_run("run-restart");
        wait_for_stop(&before_sink, 1).await;
        assert!(
            matches!(
                before_sink.events.lock().unwrap()[0].event,
                WorkerEvent::SessionStart { .. }
            ),
            "precondition: the first engine saw the session start",
        );
        // The engine process dies. Its ingress goes with it; the worker and
        // its rollout do not.
        before.stop_run("run-restart");

        let checkpoint = store
            .load_ingress_checkpoint("run-restart")
            .unwrap()
            .expect("the ingress records where it got to");
        let consumed = match &checkpoint {
            IngressCheckpoint::Attached {
                path: recorded,
                session_id,
                consumed_bytes,
                session_state,
                ..
            } => {
                assert_eq!(recorded, &fs::canonicalize(&path).unwrap());
                assert_eq!(session_id, "thread-restart");
                assert!(
                    session_state.is_some(),
                    "the driver session state belongs to the same checkpoint as the offset",
                );
                *consumed_bytes
            }
            other => panic!("expected an attached checkpoint, got {other:?}"),
        };
        assert_eq!(
            consumed,
            fs::metadata(&path).unwrap().len(),
            "the first engine consumed the whole first turn",
        );

        // The worker keeps working across the restart.
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(second_turn_text().as_bytes()).unwrap();
        }

        // Engine two: nothing in common with engine one but the checkpoint.
        let after = AgentJsonlProgressManager::new();
        let after_sink = CaptureSink::default();
        let outcome = after
            .resume_run(
                "run-restart",
                Arc::new(crate::driver::CodexDriver::default()),
                checkpoint,
                after_sink.clone(),
                store.clone(),
            )
            .expect("the recorded rollout is still attachable");
        assert_eq!(outcome, ResumeOutcome::Reestablished);

        wait_for_stop(&after_sink, 1).await;
        let events = after_sink.events.lock().unwrap();
        let kinds: Vec<&WorkerEvent> = events.iter().map(|event| &event.event).collect();
        assert!(
            !kinds
                .iter()
                .any(|event| matches!(event, WorkerEvent::SessionStart { .. })),
            "a resumed tail must not re-read the session_meta it already published: {kinds:?}"
        );
        assert!(
            matches!(kinds.first(), Some(WorkerEvent::UserPromptSubmit { session_id, .. }) if session_id == "thread-restart"),
            "the second turn opens with its own prompt, correlated to the thread the restored \
             session remembered: {kinds:?}"
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|event| matches!(event, WorkerEvent::Stop { .. }))
                .count(),
            1,
            "exactly one turn ended after the restart: {kinds:?}"
        );
        assert!(
            matches!(
                kinds.last(),
                Some(WorkerEvent::Stop {
                    stop_reason: StopReason::Completed,
                    ..
                })
            ),
            "the post-restart turn reaches a real boundary: {kinds:?}"
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|event| matches!(event, WorkerEvent::PreToolUse { .. }))
                .count(),
            1,
            "only the second turn's tool call, never the first turn's again: {kinds:?}"
        );
        drop(events);
        after.stop_run("run-restart");
    }

    /// The forbidden fallbacks, refused. A recorded rollout that is gone has
    /// no offset that means what the checkpoint meant, so the resume fails and
    /// the caller — which files an operator attention item — hears about it.
    #[tokio::test]
    async fn resuming_a_vanished_rollout_fails_loudly_rather_than_starting_over() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let manager = AgentJsonlProgressManager::new();
        let err = manager
            .resume_run(
                "run-gone",
                Arc::new(crate::driver::CodexDriver::default()),
                IngressCheckpoint::Attached {
                    ingress: AgentJsonlFileIngress {
                        directory: sessions.clone(),
                        filename_prefix: "rollout-".into(),
                        filename_suffix: ".jsonl".into(),
                        workspace_path: workspace.clone(),
                    },
                    path: sessions.join("rollout-new-thread-gone.jsonl"),
                    session_id: "thread-gone".into(),
                    consumed_bytes: 128,
                    identity: FileIdentity { device: 1, inode: 1 },
                    session_state: None,
                },
                CaptureSink::default(),
                test_store(),
            )
            .expect_err("a rollout that is not there cannot be resumed");
        assert!(err.contains("rollout-new-thread-gone.jsonl"), "got {err}");
        assert!(
            manager.runs.lock().unwrap().is_empty(),
            "a failed resume must leave no half-armed ingress behind",
        );
    }

    /// A rollout shorter than what the engine already published was truncated
    /// or replaced under the same name. Every byte offset in it now means
    /// something else, so there is nothing honest to resume from — and
    /// picking the nearest plausible offset would silently replay or silently
    /// skip.
    #[tokio::test]
    async fn resuming_a_rollout_shorter_than_what_was_consumed_fails_loudly() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let path = sessions.join("rollout-new-thread-short.jsonl");
        fs::write(&path, rollout_text(&workspace, "thread-short")).unwrap();
        let size = fs::metadata(&path).unwrap().len();

        let manager = AgentJsonlProgressManager::new();
        let err = manager
            .resume_run(
                "run-short",
                Arc::new(crate::driver::CodexDriver::default()),
                IngressCheckpoint::Attached {
                    ingress: AgentJsonlFileIngress {
                        directory: sessions.clone(),
                        filename_prefix: "rollout-".into(),
                        filename_suffix: ".jsonl".into(),
                        workspace_path: workspace.clone(),
                    },
                    path: fs::canonicalize(&path).unwrap(),
                    session_id: "thread-short".into(),
                    consumed_bytes: size + 1,
                    identity: identity_of(&path),
                    session_state: None,
                },
                CaptureSink::default(),
                test_store(),
            )
            .expect_err("an offset past the end of the file is not resumable");
        assert!(err.contains("already consumed"), "got {err}");
    }

    /// A rollout that is still there but now belongs to a different session is
    /// a different run's file wearing the recorded name.
    #[tokio::test]
    async fn resuming_a_rollout_that_lost_its_correlation_fails_loudly() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let path = sessions.join("rollout-new-thread-other.jsonl");
        fs::write(&path, rollout_text(&workspace, "thread-other")).unwrap();

        let manager = AgentJsonlProgressManager::new();
        let err = manager
            .resume_run(
                "run-other",
                Arc::new(crate::driver::CodexDriver::default()),
                IngressCheckpoint::Attached {
                    ingress: AgentJsonlFileIngress {
                        directory: sessions.clone(),
                        filename_prefix: "rollout-".into(),
                        filename_suffix: ".jsonl".into(),
                        workspace_path: workspace.clone(),
                    },
                    path: fs::canonicalize(&path).unwrap(),
                    session_id: "thread-expected".into(),
                    consumed_bytes: 0,
                    identity: identity_of(&path),
                    session_state: None,
                },
                CaptureSink::default(),
                test_store(),
            )
            .expect_err("a rollout for another session is not this run's resume point");
        assert!(err.contains("thread-expected"), "got {err}");
    }

    /// A rollout replaced under the same pathname while the engine was down
    /// is a different file, and the recorded offset indexes the dead one.
    /// Path plus length cannot see that — the replacement only has to be long
    /// enough — so the incarnation the offset was measured against is
    /// recorded and checked. Attaching anyway would skip the new file's first
    /// `consumed_bytes` bytes and land mid-line.
    #[tokio::test]
    async fn resuming_a_rollout_rotated_under_the_same_path_fails_loudly() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let path = sessions.join("rollout-new-thread-rotated.jsonl");
        fs::write(&path, rollout_text(&workspace, "thread-rotated")).unwrap();
        let dead_incarnation = identity_of(&path);
        let consumed = fs::metadata(&path).unwrap().len();

        // Replaced, not appended to: a new inode behind the same name, at
        // least as long as what the previous engine had already consumed.
        let mut replacement = rollout_text(&workspace, "thread-rotated");
        replacement.push_str(&second_turn_text());
        replace_with_new_inode(&path, &replacement);
        assert_ne!(
            identity_of(&path),
            dead_incarnation,
            "precondition: the replacement must genuinely be a different file",
        );
        assert!(
            fs::metadata(&path).unwrap().len() >= consumed,
            "precondition: the replacement must be long enough to defeat the size check alone",
        );

        let manager = AgentJsonlProgressManager::new();
        let err = manager
            .resume_run(
                "run-rotated",
                Arc::new(crate::driver::CodexDriver::default()),
                IngressCheckpoint::Attached {
                    ingress: AgentJsonlFileIngress {
                        directory: sessions.clone(),
                        filename_prefix: "rollout-".into(),
                        filename_suffix: ".jsonl".into(),
                        workspace_path: workspace.clone(),
                    },
                    path: fs::canonicalize(&path).unwrap(),
                    session_id: "thread-rotated".into(),
                    consumed_bytes: consumed,
                    identity: dead_incarnation,
                    session_state: None,
                },
                CaptureSink::default(),
                test_store(),
            )
            .expect_err("an offset into a dead incarnation is not resumable");
        assert!(err.contains("different file now"), "got {err}");
        assert!(manager.runs.lock().unwrap().is_empty());
    }

    /// The `Attached` record asserts that `consumed_bytes` is immediately past
    /// a newline. Checked rather than trusted: a truncate-and-regrow that
    /// happens to land at the same length keeps the inode and passes the size
    /// check, and attaching mid-line there splices a fragment of the dead
    /// incarnation onto the new one's next record.
    #[tokio::test]
    async fn resuming_at_an_offset_that_is_not_a_record_boundary_fails_loudly() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let path = sessions.join("rollout-new-thread-midline.jsonl");
        fs::write(&path, rollout_text(&workspace, "thread-midline")).unwrap();
        let size = fs::metadata(&path).unwrap().len();

        let manager = AgentJsonlProgressManager::new();
        let err = manager
            .resume_run(
                "run-midline",
                Arc::new(crate::driver::CodexDriver::default()),
                IngressCheckpoint::Attached {
                    ingress: AgentJsonlFileIngress {
                        directory: sessions.clone(),
                        filename_prefix: "rollout-".into(),
                        filename_suffix: ".jsonl".into(),
                        workspace_path: workspace.clone(),
                    },
                    path: fs::canonicalize(&path).unwrap(),
                    session_id: "thread-midline".into(),
                    // One byte short of the final newline: inside the last
                    // record rather than after it.
                    consumed_bytes: size - 1,
                    identity: identity_of(&path),
                    session_state: None,
                },
                CaptureSink::default(),
                test_store(),
            )
            .expect_err("an offset inside a record is not resumable");
        assert!(err.contains("does not end a record"), "got {err}");
        assert!(manager.runs.lock().unwrap().is_empty());
    }

    /// A session state the driver cannot take back is a failed resume, not a
    /// degraded one. Caught before anything attaches, so the caller can file
    /// an attention item — inside the ingress task it would only be a log
    /// line, and the run would read nothing while looking healthy.
    #[tokio::test]
    async fn resuming_with_a_session_state_the_driver_rejects_fails_loudly() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let path = sessions.join("rollout-new-thread-badstate.jsonl");
        fs::write(&path, rollout_text(&workspace, "thread-badstate")).unwrap();

        let manager = AgentJsonlProgressManager::new();
        let err = manager
            .resume_run(
                "run-badstate",
                Arc::new(crate::driver::CodexDriver::default()),
                IngressCheckpoint::Attached {
                    ingress: AgentJsonlFileIngress {
                        directory: sessions.clone(),
                        filename_prefix: "rollout-".into(),
                        filename_suffix: ".jsonl".into(),
                        workspace_path: workspace.clone(),
                    },
                    path: fs::canonicalize(&path).unwrap(),
                    session_id: "thread-badstate".into(),
                    consumed_bytes: 0,
                    identity: identity_of(&path),
                    session_state: Some(serde_json::json!("not a session snapshot")),
                },
                CaptureSink::default(),
                test_store(),
            )
            .expect_err("an unreadable session snapshot is not resumable");
        assert!(err.contains("rollout resume state"), "got {err}");
        assert!(manager.runs.lock().unwrap().is_empty());
    }

    /// A driver that never tails a file records that fact, and readoption
    /// reads it as "nothing to do" rather than having to guess from silence.
    #[tokio::test]
    async fn resuming_a_non_file_ingress_is_a_no_op_not_a_failure() {
        let manager = AgentJsonlProgressManager::new();
        let outcome = manager
            .resume_run(
                "run-hooks",
                Arc::new(crate::driver::ClaudeDriver),
                IngressCheckpoint::NotFileIngress,
                CaptureSink::default(),
                test_store(),
            )
            .unwrap();
        assert_eq!(outcome, ResumeOutcome::NotFileIngress);
        assert!(manager.runs.lock().unwrap().is_empty());
    }

    /// The reader's position and the file's are the same number right up
    /// until they are not: a resumed tail starts at a non-zero file offset,
    /// and the synthetic delimiter written on truncation/rotation is a stream
    /// byte that corresponds to no file byte at all. Getting this wrong shifts
    /// every checkpoint after the first anomaly.
    #[test]
    fn stream_positions_resolve_to_the_file_offsets_they_came_from() {
        let first = FileIdentity { device: 1, inode: 100 };
        let mut map = StreamFileMap::default();
        // A resumed tail: the reader's byte 0 is the file's byte 100.
        map.record_bytes(100, 50, first);
        assert_eq!(map.file_position_for(0).unwrap().offset, 100);
        assert_eq!(map.file_position_for(50).unwrap().offset, 150);

        // Contiguous growth stays one segment.
        map.record_bytes(150, 25, first);
        assert_eq!(map.segments.len(), 1);
        assert_eq!(map.file_position_for(75).unwrap().offset, 175);

        // The file is truncated: one stream byte, no file bytes, and the file
        // offset restarts. Truncation keeps the inode.
        map.record_delimiter(0, first);
        map.record_bytes(0, 10, first);
        assert_eq!(
            map.file_position_for(76).unwrap().offset,
            0,
            "the delimiter itself maps to the start of the new incarnation",
        );
        assert_eq!(map.file_position_for(80).unwrap().offset, 4);

        // Positions the reader cannot have reached yet clamp rather than
        // running off the end of the segment they land in.
        assert_eq!(map.file_position_for(999).unwrap().offset, 10);
    }

    /// A rotation replaces the inode mid-stream. A checkpoint taken while the
    /// reader is still behind the boundary must stay paired with the
    /// incarnation its own offset came from — pairing it with the incarnation
    /// the *tail* is now reading is the stale-offset resume this identity
    /// exists to refuse.
    #[test]
    fn positions_carry_the_incarnation_their_offset_belongs_to() {
        let old = FileIdentity { device: 1, inode: 7 };
        let new = FileIdentity { device: 1, inode: 8 };
        let mut map = StreamFileMap::default();
        map.record_bytes(0, 40, old);
        map.record_delimiter(0, new);
        map.record_bytes(0, 30, new);

        assert_eq!(map.file_position_for(10).unwrap().identity, old);
        assert_eq!(
            map.file_position_for(40).unwrap().identity,
            new,
            "the boundary itself belongs to the incarnation that follows it",
        );
        assert_eq!(map.file_position_for(60).unwrap().identity, new);
        assert_eq!(
            map.segments.len(),
            3,
            "bytes from two incarnations must never merge into one segment",
        );
    }

    /// Pruning must never move an answer the checkpointer could still ask for.
    #[test]
    fn pruning_consumed_segments_preserves_the_live_answer() {
        let identity = FileIdentity { device: 1, inode: 9 };
        let mut map = StreamFileMap::default();
        map.record_bytes(0, 10, identity);
        map.record_delimiter(0, identity);
        map.record_bytes(0, 10, identity);
        map.prune_through(21);
        assert_eq!(map.file_position_for(21).unwrap().offset, 10);
    }
}
