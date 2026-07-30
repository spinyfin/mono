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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, watch};

use crate::driver::{AgentDriver, AgentJsonlFileIngress, ProgressSessionConfig, ProgressStreamSource};
use crate::stdout_progress::WorkerEventSink;

const DISCOVERY_POLL: Duration = Duration::from_millis(100);
const FILE_POLL: Duration = Duration::from_millis(50);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DISCOVERY_DIRS: usize = 512;
const MAX_DISCOVERY_MATCHES: usize = 8;
const MAX_SESSION_META_BYTES: u64 = 64 * 1024;
const FILE_CHUNK_BYTES: usize = 64 * 1024;
const DUPLEX_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
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
        let canonical_workspace = fs::canonicalize(&ingress.workspace_path)
            .map_err(|err| format!("canonicalize workspace {}: {err}", ingress.workspace_path.display()))?;
        let baseline = scan_matching_paths(&root, &ingress)?;
        Ok(Self {
            ingress,
            root,
            canonical_workspace,
            baseline,
        })
    }
}

/// How an in-flight file ingress is being brought to an end.
///
/// The two endings are not interchangeable, which is why this is a tri-state
/// rather than the boolean it replaced. [`Self::Cancel`] is teardown: the
/// engine is releasing the pane and unread bytes are forfeit. [`Self::Drain`]
/// is the *writer* ending: the agent process has exited, so the file can never
/// grow again and every byte it holds is part of this run's record — including,
/// for a one-turn-per-process driver, the `turn.completed` envelope that says
/// the run finished cleanly. Cancelling there would discard exactly the
/// evidence [`crate::worker_process_exit`] needs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum StreamHalt {
    /// Normal operation: keep tailing the growing file.
    #[default]
    Running,
    /// The writer process has exited. Read to end of file, publish everything
    /// that is there, then close the stream. Bounded by the file's own size —
    /// there is nothing to wait for, so this terminates without a timer.
    Drain,
    /// Tear down now. Anything unread is dropped.
    Cancel,
}

struct RunHandle {
    activate: Option<oneshot::Sender<()>>,
    halt: watch::Sender<StreamHalt>,
    join: tokio::task::JoinHandle<()>,
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
    pub fn prepare_run<S>(
        &self,
        run_id: &str,
        driver: std::sync::Arc<dyn AgentDriver>,
        ingress: AgentJsonlFileIngress,
        sink: S,
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

        let prepared = PreparedSource::new(ingress)?;
        let (activate_tx, activate_rx) = oneshot::channel();
        let (halt_tx, halt_rx) = watch::channel(StreamHalt::Running);
        let task_run_id = run_id.to_owned();
        let join = tokio::spawn(async move {
            run_prepared(task_run_id, driver, prepared, sink, activate_rx, halt_rx).await;
        });
        runs.insert(
            run_id.to_owned(),
            RunHandle {
                activate: Some(activate_tx),
                halt: halt_tx,
                join,
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
    /// A caller that is reacting to the *writer* exiting wants
    /// [`Self::finish_run`] instead.
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

    /// Signal that the run's **writer has exited**, so the source file is
    /// final: read it to end of file, dispatch every remaining event through
    /// the sink, then close. Returns the ingress task's join handle so the
    /// caller can await that completion.
    ///
    /// This is the ordering primitive behind the one-turn-per-process exit
    /// check. It is causal, not temporal: the agent process exiting is what
    /// makes the file final, so "read to EOF" terminates on the file's own
    /// size with nothing to wait for — it is not a grace window that hopes a
    /// racing event turns up. When the returned handle resolves, every event
    /// the run ever produced has been through the engine's fan-out, so
    /// `turn.completed` has already reached the completion handler and the
    /// durable turn-boundary record is written.
    ///
    /// Returns `None` when no ingress is registered for `run_id` — a driver
    /// with a different progress transport (Claude's hook callback), a run
    /// whose ingress already ended, or one that never started. Callers must
    /// treat `None` as "nothing to drain", never as "the run delivered
    /// nothing": the verdict comes from the durable record, not from here.
    pub fn finish_run(&self, run_id: &str) -> Option<tokio::task::JoinHandle<()>> {
        let Ok(mut runs) = self.runs.lock() else {
            tracing::warn!(run_id, "agent JSONL progress: manager mutex poisoned during finish");
            return None;
        };
        let handle = runs.remove(run_id)?;
        let _ = handle.halt.send(StreamHalt::Drain);
        // Dropping the activate sender makes a never-activated ingress return
        // immediately rather than block the caller until its discovery timeout.
        drop(handle.activate);
        Some(handle.join)
    }
}

async fn run_prepared<S>(
    run_id: String,
    driver: std::sync::Arc<dyn AgentDriver>,
    prepared: PreparedSource,
    sink: S,
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

    let candidate = match discover_candidate(&prepared, &mut halt).await {
        Ok(Some(candidate)) => candidate,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(run_id, %err, "agent JSONL progress: discovery failed");
            return;
        }
    };
    tracing::info!(
        run_id,
        session_id = %candidate.session_id,
        path = %candidate.path.display(),
        "agent JSONL progress: attached rollout",
    );
    let transcript_path = candidate.path.clone();

    let (reader, writer) = tokio::io::duplex(DUPLEX_BYTES);
    let tail_halt = halt.clone();
    let tail_source = prepared.clone();
    let tail = tokio::spawn(async move {
        if let Err(err) = stream_file_bytes(tail_source, candidate, writer, tail_halt).await {
            tracing::warn!(%err, "agent JSONL progress: file tail ended with error");
        }
    });

    let config = ProgressSessionConfig {
        run_id: Some(run_id.clone()),
        identity_store: sink.progress_identity_store(),
        source: ProgressStreamSource::AgentJsonlFile,
        transcript_path: Some(transcript_path),
    };
    let driver_slug = driver.descriptor().name;
    let _stats = crate::stdout_progress::run_jsonl_progress_ingress_with_driver(
        &run_id,
        driver_slug,
        driver,
        reader,
        &sink,
        config,
    )
    .await;
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
        // Either ending stops discovery. A `Drain` here means the writer
        // exited before it ever created a correlated rollout — there is
        // nothing to read to EOF, and the absent turn-boundary record is
        // exactly what makes that exit a death.
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
    writer: tokio::io::DuplexStream,
    halt: watch::Receiver<StreamHalt>,
) -> Result<(), String> {
    stream_file_bytes_with_test_hooks(prepared, candidate, writer, halt, |_| Ok(()), |_| Ok(())).await
}

/// Stream only descriptors returned by [`validate_candidate`].
///
/// The hooks are deterministic adversarial-test seams immediately before a
/// descriptor read and between validating and installing a rotation.
/// Production passes no-ops. They do not change the validation path exercised
/// by tests.
async fn stream_file_bytes_with_test_hooks<F, G>(
    prepared: PreparedSource,
    candidate: Candidate,
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
    let mut offset = 0u64;
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
                offset += count as u64;
                write_or_halt(&mut writer, &buffer[..count], &mut halt).await?;
                continue;
            }
        }

        // Everything the file held up to `size` has now been published. Under
        // `Drain` the writer process has already exited, so the file cannot
        // grow again and this is genuinely end-of-stream: close it rather than
        // poll a file nobody is writing. Placed AFTER the read above so a
        // drain can never truncate the run's own terminal envelope — which is
        // the whole point of draining instead of cancelling.
        if *halt.borrow() == StreamHalt::Drain {
            return Ok(());
        }

        let path_metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tokio::select! {
                    _ = tokio::time::sleep(FILE_POLL) => continue,
                    changed = halt.changed() => {
                        // Every sender dropped: nothing can ever halt us. End
                        // cleanly rather than spinning the poll loop forever.
                        // A prior `Drain`/`Cancel` still reaches us via
                        // `changed()`'s last Ok (version bumps before close).
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
                // Re-run the loop so a `Drain` re-reads the file (picking up
                // bytes written between the last poll and the writer's exit)
                // before the drain check above closes the stream; the
                // top-of-loop check handles `Cancel`. Every sender dropped
                // is terminal: end cleanly rather than busy-looping the poll.
                // A prior `Drain` still arrives as Ok first (version before
                // the closed flag).
                if changed.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Publish `bytes` to the reader, abandoning the write only on
/// [`StreamHalt::Cancel`].
///
/// A `Drain` transition deliberately does **not** cancel an in-flight write:
/// these bytes have already been read off the run's now-final file, and
/// `write_all` is not cancellation-safe — dropping it mid-write would lose a
/// partial chunk, corrupting the very envelope the drain exists to deliver.
/// The consumer is actively reading to EOF during a drain, so the await
/// completes rather than hangs.
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

/// Resolve only once the halt state reaches [`StreamHalt::Cancel`], so a
/// `Running` → `Drain` transition never wins a `select!` against a write.
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

    /// [`rollout_text`] minus its final `task_complete` record — the state of
    /// a rollout mid-turn, before the envelope that ends it.
    fn rollout_text_without_task_complete(workspace: &Path, thread_id: &str) -> String {
        let full = rollout_text(workspace, thread_id);
        let mut lines: Vec<&str> = full.lines().collect();
        lines.pop();
        lines.join("\n") + "\n"
    }

    /// Just the `task_complete` line — appended to simulate `codex exec`
    /// writing its terminal envelope immediately before the process exits.
    fn task_complete_line() -> String {
        serde_json::to_string(&serde_json::json!({
            "type":"event_msg",
            "payload":{
                "type":"task_complete",
                "turn_id":"turn-live",
                "last_agent_message":"done"
            }
        }))
        .unwrap()
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

    /// The ordering guarantee `worker_process_exit` rests on: when the agent
    /// process exits its rollout is final, so `finish_run` must read whatever
    /// is left of it and put every remaining event through the fan-out before
    /// its handle resolves.
    ///
    /// The terminal envelope is appended and the drain requested with no wait
    /// in between — exactly the shape of the live failure, where `codex exec`
    /// wrote `task_complete` and exited 160 ms before anything read it. Under
    /// `stop_run`'s cancel those bytes would simply be dropped.
    #[tokio::test]
    async fn finish_run_drains_the_final_envelope_before_its_handle_resolves() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&workspace).unwrap();

        let manager = AgentJsonlProgressManager::new();
        let sink = CaptureSink::default();
        manager
            .prepare_run(
                "run-drain",
                Arc::new(crate::driver::CodexDriver::default()),
                AgentJsonlFileIngress {
                    directory: sessions.clone(),
                    filename_prefix: "rollout-".into(),
                    filename_suffix: ".jsonl".into(),
                    workspace_path: workspace.clone(),
                },
                sink.clone(),
            )
            .unwrap();

        let path = sessions.join("rollout-new-thread-drain.jsonl");
        fs::write(&path, rollout_text_without_task_complete(&workspace, "thread-drain")).unwrap();
        manager.activate_run("run-drain");

        // Let the tail attach and consume the mid-turn records, so the only
        // thing the drain can be responsible for is the terminal envelope.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if sink.events.lock().unwrap().len() >= 4 {
                    break;
                }
                sink.notify.notified().await;
            }
        })
        .await
        .expect("mid-turn rollout events should reach the fanout");
        assert!(
            !sink
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event.event, WorkerEvent::Stop { .. })),
            "precondition: no turn boundary has been observed yet",
        );

        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(task_complete_line().as_bytes()).unwrap();
        }

        let join = manager.finish_run("run-drain").expect("an active ingress to drain");
        tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("draining a final file must terminate on the file's own size")
            .unwrap();

        let events = sink.events.lock().unwrap();
        assert!(
            events.iter().any(|event| matches!(
                &event.event,
                WorkerEvent::Stop {
                    stop_reason: StopReason::Completed,
                    ..
                }
            )),
            "the terminal envelope must have been fanned out by the time the drain resolved; got: {:?}",
            events.iter().map(|e| &e.event).collect::<Vec<_>>(),
        );
    }

    /// `finish_run` is keyed on a live ingress, and returning `None` must mean
    /// "nothing to drain" — never "the run delivered nothing". Callers read
    /// the durable turn-boundary record for that.
    #[tokio::test]
    async fn finish_run_is_none_for_an_unknown_run() {
        let manager = AgentJsonlProgressManager::new();
        assert!(manager.finish_run("run-never-prepared").is_none());
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
                )
                .unwrap();
        }
        assert_eq!(manager.runs.lock().unwrap().len(), 1);
        manager.stop_run("run-duplicate");
        assert!(manager.runs.lock().unwrap().is_empty());
    }
}
