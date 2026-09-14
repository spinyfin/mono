//! Rollout discovery for [`crate::agent_jsonl_progress`]: find the one new,
//! run-correlated rollout file under a run's watched root, and when none can
//! be attached, say exactly what was seen instead.
//!
//! ## The failure this exists to make visible
//!
//! On 2026-09-13 four live, productive Codex workers were reaped as
//! "driver-start timeouts" by [`crate::spawn_ack_sweep`]. Their rollouts
//! were on disk and still being appended to at the moment of the reap. One
//! of them had been created 43 seconds before discovery's own deadline, and
//! the loop — which re-scans the root every 100 ms — never attached it.
//!
//! The loop's only failure message was
//! `no correlated rollout appeared under {root} within {n}s`. It was emitted
//! identically whether the directory had been empty for the whole window or a
//! rollout had been present and inspected on every scan but rejected by
//! correlation: a `session_meta` first line longer than the read cap, a `cwd`
//! that does not canonicalize to the workspace, a filename that does not
//! carry the session id. Every rejection was swallowed
//! (`validate_candidate(..).ok().flatten()`), so a present-but-rejected file
//! was indistinguishable from an absent one — and downstream, the resulting
//! silence on the ingress was read as proof that the driver never ran.
//!
//! This module keeps the same correlation rules and changes what happens
//! around them:
//!
//! - Every candidate that is seen but not attached carries a
//!   [`CandidateRejection`] saying why, logged the first time each
//!   `(path, reason)` pair is observed rather than after the window closes.
//! - The scan runs on the blocking pool ([`tokio::task::spawn_blocking`]),
//!   not on an async worker thread. A scan is `read_dir` + `canonicalize` +
//!   `open` + a bounded read per matching file, ten times a second per run;
//!   under a burst of concurrent spawns that was runtime time the engine's
//!   other tasks — including the scheduler — did not get.
//! - The loop measures its own cadence. A gap between consecutive scans well
//!   above the poll interval is logged as starvation evidence, and the count
//!   of scans and the longest gap are part of the failure message, so "the
//!   poll never ran" is a claim the log can confirm or refute.
//! - The failure message names what discovery observed: either that no file
//!   with the rollout name shape existed under the root at any scan, or which
//!   files existed and why each was rejected on the final scan. The phrase
//!   "no correlated rollout appeared" is gone; it claimed absence for a file
//!   the loop had opened hundreds of times.
//!
//! The reaper consults the same scan through [`probe_correlated_rollout`],
//! so a reap decision can look at the rollout directly instead of inferring
//! from whether the ingress ever produced an event.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::agent_jsonl_progress::{
    Candidate, IngressCheckpoint, IngressCheckpointStore, PreparedSource, StreamHalt, VerifiedRoot,
    validate_candidate_explained,
};
use crate::driver::AgentJsonlFileIngress;

/// How often discovery re-scans the root while waiting for the rollout.
pub(crate) const DISCOVERY_POLL: Duration = Duration::from_millis(100);
/// How long discovery waits for a correlated rollout before giving up.
///
/// Deliberately unchanged by the 2026-09-13 fix: a wider window does not help
/// a scan that saw the file and rejected it, and the reaper no longer treats
/// discovery's silence as evidence of anything.
pub(crate) const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
/// A gap between two consecutive scans above this is logged as evidence that
/// the discovery task was not being scheduled. Twenty poll intervals: far
/// above jitter, far below the window.
const STARVED_SCAN_GAP: Duration = Duration::from_secs(2);
const MAX_DISCOVERY_DIRS: usize = 512;
const MAX_DISCOVERY_MATCHES: usize = 8;

/// Why a file under the watched root, with the right name shape, was not
/// attached as this run's rollout.
///
/// Each variant is one check in the correlation chain. The `Display` text is
/// what an operator reads in the discovery failure and in the reaper's
/// liveness summary, so it says what was found, not just which check failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateRejection {
    /// Present before this run's pane was spawned — a previous incarnation's
    /// rollout, excluded by the pre-spawn baseline.
    InBaseline,
    /// A symlink, a directory, or a hard-linked file.
    NotSingleLinkRegularFile,
    /// Canonicalizes to somewhere outside the verified root.
    OutsideRoot,
    /// The path was replaced or relinked between two looks at it.
    IdentityChanged,
    /// Zero bytes: created, nothing written yet.
    Empty,
    /// The first line has no newline yet and is shorter than the read cap —
    /// the writer is still on it.
    SessionMetaUnterminated { bytes: u64 },
    /// The first line reached the read cap without a newline. A
    /// `session_meta` longer than the cap can never correlate, so this is a
    /// permanent rejection, not a transient one.
    SessionMetaOversized { cap_bytes: u64 },
    /// The first record is not a `session_meta`.
    NotSessionMeta { record_type: Option<String> },
    /// The `session_meta` has no `payload` object.
    MissingPayload,
    /// The `session_meta` payload has no string `id`.
    MissingSessionId,
    /// Re-validation of an attached file found a different session id.
    SessionIdMismatch { found: String, expected: String },
    /// The `session_meta` payload has no string `cwd`.
    MissingCwd,
    /// The `cwd` does not exist or cannot be canonicalized.
    CwdNotResolvable { cwd: String },
    /// The `cwd` canonicalizes to a directory other than the run's workspace.
    CwdMismatch { cwd: String, expected: PathBuf },
    /// The filename does not end in `-<session id><suffix>`.
    NameMismatch { expected_suffix: String },
    /// An I/O error while validating — the file may have vanished between
    /// the directory scan and the open, or be unreadable.
    Unreadable(String),
}

impl CandidateRejection {
    /// Whether the same file could still correlate on a later scan.
    ///
    /// Transient rejections are the normal shape of a rollout being written;
    /// a permanent one means waiting longer cannot change the answer, and the
    /// failure message says so.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            CandidateRejection::Empty
                | CandidateRejection::SessionMetaUnterminated { .. }
                | CandidateRejection::IdentityChanged
                | CandidateRejection::Unreadable(_)
        )
    }
}

impl fmt::Display for CandidateRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CandidateRejection::InBaseline => {
                write!(
                    f,
                    "existed before this run's pane was spawned (in the pre-spawn baseline)"
                )
            }
            CandidateRejection::NotSingleLinkRegularFile => {
                write!(f, "not a single-link regular file")
            }
            CandidateRejection::OutsideRoot => write!(f, "resolves outside the watched root"),
            CandidateRejection::IdentityChanged => {
                write!(f, "path identity changed while it was being validated")
            }
            CandidateRejection::Empty => write!(f, "empty (created, nothing written yet)"),
            CandidateRejection::SessionMetaUnterminated { bytes } => {
                write!(f, "first line not yet newline-terminated ({bytes} bytes so far)")
            }
            CandidateRejection::SessionMetaOversized { cap_bytes } => write!(
                f,
                "first line exceeds the {cap_bytes}-byte session_meta read cap without a newline; \
                 a session_meta this long can never correlate"
            ),
            CandidateRejection::NotSessionMeta { record_type } => write!(
                f,
                "first record is {} rather than session_meta",
                record_type
                    .as_deref()
                    .map_or("untyped".to_owned(), |t| format!("`{t}`"))
            ),
            CandidateRejection::MissingPayload => write!(f, "session_meta has no payload object"),
            CandidateRejection::MissingSessionId => write!(f, "session_meta payload has no string `id`"),
            CandidateRejection::SessionIdMismatch { found, expected } => {
                write!(f, "session_meta reports session {found} rather than {expected}")
            }
            CandidateRejection::MissingCwd => write!(f, "session_meta payload has no string `cwd`"),
            CandidateRejection::CwdNotResolvable { cwd } => {
                write!(f, "session_meta cwd `{cwd}` does not exist or cannot be canonicalized")
            }
            CandidateRejection::CwdMismatch { cwd, expected } => write!(
                f,
                "session_meta cwd `{cwd}` is not the run's workspace `{}`",
                expected.display()
            ),
            CandidateRejection::NameMismatch { expected_suffix } => {
                write!(
                    f,
                    "filename does not end in `{expected_suffix}` (the session id from its session_meta)"
                )
            }
            CandidateRejection::Unreadable(err) => write!(f, "could not be validated: {err}"),
        }
    }
}

/// One complete look at the root: every file with the rollout name shape,
/// partitioned into the ones that correlate and the ones that do not.
#[derive(Debug, Default)]
pub(crate) struct DiscoveryScan {
    pub(crate) matched: Vec<Candidate>,
    pub(crate) rejected: Vec<(PathBuf, CandidateRejection)>,
}

/// Walk the root once and validate every file with the rollout name shape.
///
/// Blocking: call it from [`tokio::task::spawn_blocking`] or a plain thread.
/// A validation I/O error on one file is recorded as
/// [`CandidateRejection::Unreadable`] for that file rather than failing the
/// whole scan — the file may simply have vanished between `read_dir` and
/// `open`, and the other files still deserve a verdict.
pub(crate) fn scan_once(prepared: &PreparedSource) -> Result<DiscoveryScan, String> {
    prepared.root.revalidate()?;
    let mut paths: Vec<PathBuf> = scan_matching_paths(&prepared.root, &prepared.ingress)?
        .into_iter()
        .collect();
    paths.sort();
    let mut scan = DiscoveryScan::default();
    for path in paths {
        if prepared.baseline.contains(&path) {
            scan.rejected.push((path, CandidateRejection::InBaseline));
            continue;
        }
        match validate_candidate_explained(prepared, &path) {
            Ok(Ok(candidate)) => scan.matched.push(candidate),
            Ok(Err(rejection)) => scan.rejected.push((path, rejection)),
            Err(err) => scan.rejected.push((path, CandidateRejection::Unreadable(err))),
        }
    }
    Ok(scan)
}

/// What the loop measured about its own scheduling, for the failure message
/// and the starvation warning.
#[derive(Debug, Clone, Copy)]
struct ScanCadence {
    scans: u64,
    longest_gap: Duration,
    last_scan_finished: tokio::time::Instant,
}

impl ScanCadence {
    fn start(now: tokio::time::Instant) -> Self {
        Self {
            scans: 0,
            longest_gap: Duration::ZERO,
            last_scan_finished: now,
        }
    }

    /// Record a completed scan; returns the gap since the previous one.
    fn record(&mut self, now: tokio::time::Instant) -> Duration {
        let gap = now.saturating_duration_since(self.last_scan_finished);
        self.scans += 1;
        self.longest_gap = self.longest_gap.max(gap);
        self.last_scan_finished = now;
        gap
    }
}

/// Why discovery gave up, stated as what it observed.
///
/// Two shapes, deliberately distinct: nothing with the rollout name shape
/// ever existed under the root, or something did and was rejected. The
/// second names every file seen on the final scan with its reason, and says
/// whether the reason is one that more waiting could have changed.
#[derive(Debug)]
pub(crate) struct DiscoveryFailure {
    root: PathBuf,
    /// The filename shape discovery was looking for, e.g. `rollout-*.jsonl`.
    name_shape: String,
    elapsed: Duration,
    cadence: ScanCadence,
    rejected: Vec<(PathBuf, CandidateRejection)>,
}

impl fmt::Display for DiscoveryFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scans = self.cadence.scans;
        let gap_ms = self.cadence.longest_gap.as_millis();
        let secs = self.elapsed.as_secs();
        if self.rejected.is_empty() {
            return write!(
                f,
                "no file named {} existed under {} on any of {scans} scans over {secs}s \
                 (longest gap between scans {gap_ms}ms)",
                self.name_shape,
                self.root.display(),
            );
        }
        write!(
            f,
            "{} rollout file(s) exist under {} but none correlates to this run after {scans} scans \
             over {secs}s (longest gap between scans {gap_ms}ms):",
            self.rejected.len(),
            self.root.display(),
        )?;
        for (path, rejection) in &self.rejected {
            let kind = if rejection.is_transient() {
                "transient"
            } else {
                "permanent"
            };
            write!(f, " [{kind}] {}: {rejection};", path.display())?;
        }
        Ok(())
    }
}

/// Wait for exactly one new, correlated rollout under the prepared root.
///
/// `Ok(None)` is a halt: the engine tore the ingress down. `Err` is a
/// diagnosis — see [`DiscoveryFailure`] — never a bare "nothing appeared".
///
/// Every scan runs on the blocking pool. The deadline is checked only after a
/// completed scan, so a task that wakes late still looks at the root before
/// it concludes anything; the failure carries the scan count and the longest
/// inter-scan gap so a late wake is visible in the log rather than inferred.
pub(crate) async fn discover_candidate(
    prepared: &PreparedSource,
    halt: &mut watch::Receiver<StreamHalt>,
    timeout: Duration,
) -> Result<Option<Candidate>, String> {
    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    let prepared = Arc::new(prepared.clone());
    let mut cadence = ScanCadence::start(started);
    let mut reported: HashSet<(PathBuf, String)> = HashSet::new();
    // Every distinct rejected path ever seen across the whole window, keyed
    // by path so a later scan's verdict for the same path supersedes an
    // earlier one. The failure message is built from this, not from the
    // final scan's `rejected` alone — a file that was seen and rejected
    // earlier in the window and then rotated, renamed, or removed before the
    // last scan must still be named in the diagnosis, not silently dropped
    // back to "nothing appeared".
    let mut ever_rejected: std::collections::HashMap<PathBuf, CandidateRejection> = std::collections::HashMap::new();
    loop {
        // A `Cancel` during discovery stops it: the engine is tearing the
        // ingress down and there is nothing left to attach to.
        if *halt.borrow() != StreamHalt::Running {
            return Ok(None);
        }
        let scan_source = Arc::clone(&prepared);
        let scan = tokio::task::spawn_blocking(move || scan_once(&scan_source))
            .await
            .map_err(|err| format!("rollout discovery scan task failed: {err}"))??;
        let now = tokio::time::Instant::now();
        let gap = cadence.record(now);
        if gap > STARVED_SCAN_GAP && cadence.scans > 1 {
            tracing::warn!(
                root = %prepared.root.path.display(),
                gap_ms = gap.as_millis() as u64,
                poll_ms = DISCOVERY_POLL.as_millis() as u64,
                scans = cadence.scans,
                "agent JSONL progress: discovery poll starved — the gap between two scans was far \
                 above the poll interval; the engine runtime was not scheduling this task",
            );
        }
        for (path, rejection) in &scan.rejected {
            // Log each distinct (path, reason) once, when it is first seen —
            // during the window, not after it. A permanent rejection at
            // T+1s is the whole diagnosis; nothing is gained by waiting the
            // window out before saying so.
            if reported.insert((path.clone(), rejection.to_string())) {
                tracing::warn!(
                    path = %path.display(),
                    transient = rejection.is_transient(),
                    %rejection,
                    "agent JSONL progress: a rollout file exists under the watched root but does not \
                     correlate to this run",
                );
            }
            ever_rejected.insert(path.clone(), rejection.clone());
        }
        let DiscoveryScan { mut matched, .. } = scan;
        match matched.len() {
            1 => return Ok(matched.pop()),
            count if count > 1 => {
                return Err(format!(
                    "{count} new rollout files matched one run; refusing ambiguous attachment"
                ));
            }
            _ => {}
        }
        // Checked after the scan, never before it: the scan that precedes
        // this test is the authoritative final look at the root.
        if now >= deadline {
            let mut rejected: Vec<(PathBuf, CandidateRejection)> = ever_rejected.into_iter().collect();
            rejected.sort_by(|a, b| a.0.cmp(&b.0));
            return Err(DiscoveryFailure {
                root: prepared.root.path.clone(),
                name_shape: format!(
                    "{}*{}",
                    prepared.ingress.filename_prefix, prepared.ingress.filename_suffix
                ),
                elapsed: now.saturating_duration_since(started),
                cadence,
                rejected,
            }
            .to_string());
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

/// Every file under `root` whose name has the rollout shape, canonicalized.
/// Bounded in directories visited and files matched so a hostile or runaway
/// tree cannot turn a 100 ms poll into an unbounded walk.
pub(crate) fn scan_matching_paths(
    root: &VerifiedRoot,
    ingress: &AgentJsonlFileIngress,
) -> Result<HashSet<PathBuf>, String> {
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
        // NOTE: every error branch below that is not `NotFound` returns
        // `Err`, failing the whole scan, rather than `continue`-ing past the
        // entry. A silently dropped entry never becomes a `CandidateRejection`
        // — it appears in neither `matched` nor `rejected` — so a scan that
        // could not fully inspect the directory would otherwise look
        // identical to one that inspected it and found nothing, and
        // `probe_correlated_rollout` would read that as `Absent` and
        // authorize a reap against a rollout it never actually looked at.
        // `scan_once`'s callers already turn a whole-scan `Err` into
        // `Undeterminable`, which is the honest answer here.
        for entry in entries {
            let entry = entry.map_err(|err| format!("read a directory entry under {}: {err}", dir.display()))?;
            let file_type = entry
                .file_type()
                .map_err(|err| format!("file type of {}: {err}", entry.path().display()))?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let name_shaped = name.starts_with(&ingress.filename_prefix) && name.ends_with(&ingress.filename_suffix);
            if file_type.is_symlink() {
                // Never followed, whether it points at a file or a
                // directory (following a symlinked directory risks a
                // traversal loop). A symlink whose name has the rollout
                // shape is still a candidate, though: recorded as-is (not
                // canonicalized — canonicalizing would silently resolve it
                // to its target's identity) so `validate_candidate_explained`
                // rejects it with `NotSingleLinkRegularFile` instead of it
                // vanishing from both `matched` and `rejected`.
                if name_shaped {
                    matches.insert(path);
                }
                continue;
            }
            if file_type.is_dir() {
                let canonical = match fs::canonicalize(&path) {
                    Ok(canonical) => canonical,
                    // Raced away between `read_dir` and `canonicalize` —
                    // legitimately gone, not an inspection failure.
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(format!("canonicalize {}: {err}", path.display())),
                };
                if canonical.starts_with(&root.canonical) {
                    stack.push(canonical);
                }
                continue;
            }
            if !file_type.is_file() || !name_shaped {
                continue;
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(canonical) => canonical,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(format!("canonicalize {}: {err}", path.display())),
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

// ─── liveness probe ──────────────────────────────────────────────────────────

/// What the filesystem says about a run's rollout, asked directly.
///
/// This is the question [`crate::spawn_ack_sweep`] must ask before it reaps a
/// slot for having produced no driver signal. The ingress's silence is not
/// an answer to it: discovery can fail against a rollout that exists (the
/// whole point of this module), and a rollout that exists was written by the
/// driver, which is exactly the "did the driver start?" evidence the sweep is
/// looking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolloutLiveness {
    /// A rollout newer than the pre-spawn baseline exists under the run's
    /// watched root. `age_secs` is now minus its mtime; `correlation` is the
    /// same verdict discovery applies — `Err` means the file is the driver's
    /// but discovery would not attach it, which is a bug to fix, not a
    /// reason to reap.
    Present {
        path: PathBuf,
        age_secs: i64,
        bytes: u64,
        correlation: Result<String, CandidateRejection>,
        /// How many new rollout files exist in total; the fields above
        /// describe the most recently modified one.
        new_files: usize,
    },
    /// The run has a file ingress and nothing beyond the baseline exists under
    /// its root.
    Absent { root: PathBuf, baseline_files: usize },
    /// The run's driver does not tail a file (its progress arrives over the
    /// hook socket or its own stdout); there is no rollout to look for.
    NotFileIngress,
    /// No ingress checkpoint was ever recorded for the run: no engine armed
    /// one, so the spawn never got as far as preparing progress observation.
    NeverArmed,
    /// The answer could not be established. The reason names what could
    /// not be read; a caller that would have reaped must not.
    Undeterminable(String),
}

/// Ask the filesystem whether `run_id`'s rollout exists.
///
/// Reads the run's durable [`IngressCheckpoint`] to learn where the rollout
/// would be and what pre-dated the spawn, then scans — the same scan
/// discovery uses, so the two can never disagree about which files are
/// candidates. Synchronous and blocking; the sweeps that call it already run
/// blocking DB work inline.
pub fn probe_correlated_rollout(
    store: &dyn IngressCheckpointStore,
    run_id: &str,
    now_epoch_secs: i64,
) -> RolloutLiveness {
    let checkpoint = match store.load_ingress_checkpoint(run_id) {
        Ok(Some(checkpoint)) => checkpoint,
        Ok(None) => return RolloutLiveness::NeverArmed,
        Err(err) => {
            return RolloutLiveness::Undeterminable(format!("the run's ingress checkpoint could not be read: {err}"));
        }
    };
    let (prepared, expected_path) = match checkpoint {
        IngressCheckpoint::NotFileIngress => return RolloutLiveness::NotFileIngress,
        IngressCheckpoint::Armed { ingress, baseline } => {
            match PreparedSource::with_baseline(ingress, baseline.into_iter().collect()) {
                Ok(prepared) => (prepared, None),
                Err(err) => {
                    return RolloutLiveness::Undeterminable(format!(
                        "the run's rollout root could not be verified: {err}"
                    ));
                }
            }
        }
        IngressCheckpoint::Attached { ingress, path, .. } => {
            match PreparedSource::with_baseline(ingress, HashSet::new()) {
                Ok(prepared) => (prepared, Some(path)),
                Err(err) => {
                    return RolloutLiveness::Undeterminable(format!(
                        "the run's rollout root could not be verified: {err}"
                    ));
                }
            }
        }
    };
    let scan = match scan_once(&prepared) {
        Ok(scan) => scan,
        Err(err) => {
            return RolloutLiveness::Undeterminable(format!(
                "the run's rollout root {} could not be scanned: {err}",
                prepared.root.path.display()
            ));
        }
    };
    let mut baseline_files = 0usize;
    let mut new_files: Vec<(PathBuf, Result<String, CandidateRejection>)> = Vec::new();
    for candidate in scan.matched {
        new_files.push((candidate.path, Ok(candidate.session_id)));
    }
    for (path, rejection) in scan.rejected {
        if rejection == CandidateRejection::InBaseline {
            baseline_files += 1;
        } else {
            new_files.push((path, Err(rejection)));
        }
    }
    if let Some(expected) = expected_path
        && !new_files.iter().any(|(path, _)| *path == expected)
    {
        return RolloutLiveness::Undeterminable(format!(
            "the rollout the ingress attached to, {}, is no longer under the run's root",
            expected.display()
        ));
    }
    let mut newest: Option<(PathBuf, Result<String, CandidateRejection>, i64, u64)> = None;
    for (path, correlation) in &new_files {
        let (age_secs, bytes) = match file_age_and_size(path, now_epoch_secs) {
            Ok(measured) => measured,
            Err(err) => {
                return RolloutLiveness::Undeterminable(format!(
                    "rollout {} exists but could not be stat'ed: {err}",
                    path.display()
                ));
            }
        };
        if newest
            .as_ref()
            .is_none_or(|(_, _, newest_age, _)| age_secs < *newest_age)
        {
            newest = Some((path.clone(), correlation.clone(), age_secs, bytes));
        }
    }
    match newest {
        Some((path, correlation, age_secs, bytes)) => RolloutLiveness::Present {
            path,
            age_secs,
            bytes,
            correlation,
            new_files: new_files.len(),
        },
        None => RolloutLiveness::Absent {
            root: prepared.root.path.clone(),
            baseline_files,
        },
    }
}

/// Seconds since `path` was last modified (clamped at zero), and its size.
pub(crate) fn file_age_and_size(path: &Path, now_epoch_secs: i64) -> Result<(i64, u64), String> {
    let metadata = fs::symlink_metadata(path).map_err(|err| format!("stat {}: {err}", path.display()))?;
    let modified = metadata
        .modified()
        .map_err(|err| format!("mtime {}: {err}", path.display()))?;
    let modified_epoch = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok((now_epoch_secs.saturating_sub(modified_epoch).max(0), metadata.len()))
}

impl fmt::Display for RolloutLiveness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RolloutLiveness::Present {
                path,
                age_secs,
                bytes,
                correlation,
                new_files,
            } => {
                write!(
                    f,
                    "rollout {} exists ({bytes} bytes, last written {age_secs}s ago",
                    path.display()
                )?;
                if *new_files > 1 {
                    write!(f, ", newest of {new_files} new rollout files")?;
                }
                match correlation {
                    Ok(session_id) => write!(f, ", session {session_id}, correlates to the run)"),
                    Err(rejection) => write!(f, "; discovery would NOT attach it: {rejection})"),
                }
            }
            RolloutLiveness::Absent { root, baseline_files } => write!(
                f,
                "no rollout file newer than the pre-spawn baseline exists under {} ({baseline_files} \
                 pre-existing file(s) excluded)",
                root.display()
            ),
            RolloutLiveness::NotFileIngress => {
                write!(f, "the driver does not write a rollout file (hook or stdout progress)")
            }
            RolloutLiveness::NeverArmed => {
                write!(f, "no progress ingress was ever armed for the run")
            }
            RolloutLiveness::Undeterminable(reason) => {
                write!(f, "could not be determined: {reason}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;

    fn session_meta_line(session_id: &str, cwd: &Path) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "timestamp": "2026-09-13T21:41:42.000Z",
                "type": "session_meta",
                "payload": {
                    "id": session_id,
                    "timestamp": "2026-09-13T21:41:42.000Z",
                    "cwd": cwd.display().to_string(),
                    "originator": "codex_cli_rs",
                    "cli_version": "0.0.0-test",
                }
            })
        )
    }

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
        workspace: PathBuf,
        other_dir: PathBuf,
        ingress: AgentJsonlFileIngress,
    }

    fn fixture() -> Fixture {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        let other_dir = temp.path().join("elsewhere");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&other_dir).unwrap();
        let ingress = AgentJsonlFileIngress {
            directory: root.clone(),
            filename_prefix: "rollout-".to_owned(),
            filename_suffix: ".jsonl".to_owned(),
            workspace_path: workspace.clone(),
        };
        Fixture {
            _temp: temp,
            root,
            workspace,
            other_dir,
            ingress,
        }
    }

    fn running_halt() -> watch::Receiver<StreamHalt> {
        let (tx, rx) = watch::channel(StreamHalt::Running);
        // Keep the sender alive for the test's duration by leaking it: a
        // dropped sender would end discovery with `Ok(None)`.
        std::mem::forget(tx);
        rx
    }

    /// The 43-second-margin shape: a rollout is present and inspected on
    /// every scan but rejected by correlation. The failure must name the
    /// file and the reason, and must not claim nothing appeared.
    #[tokio::test]
    async fn failure_names_a_present_but_rejected_rollout() {
        let fx = fixture();
        let prepared = PreparedSource::new(fx.ingress.clone()).unwrap();
        let rollout = fx.root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl");
        fs::write(&rollout, session_meta_line("sess-1", &fx.other_dir)).unwrap();

        let err = discover_candidate(&prepared, &mut running_halt(), Duration::from_millis(350))
            .await
            .expect_err("a rejected rollout is a failure, not an attachment");

        assert!(
            err.starts_with("1 rollout file(s) exist under"),
            "the failure must say a file existed; got: {err}"
        );
        assert!(err.contains("rollout-2026-09-13T21-41-42-sess-1.jsonl"), "got: {err}");
        assert!(
            err.contains("is not the run's workspace"),
            "the failure must carry the rejection reason; got: {err}"
        );
        assert!(
            err.contains("[permanent]"),
            "a cwd mismatch cannot resolve itself; got: {err}"
        );
        assert!(!err.contains("no correlated rollout appeared"), "got: {err}");
        assert!(!err.contains("no file named"), "got: {err}");
        assert!(
            err.contains("scans over 0s"),
            "the failure must report how often it looked; got: {err}"
        );
    }

    /// When the root stayed empty the failure says exactly that, with the
    /// scan count and cadence so a starved poll is visible.
    #[tokio::test]
    async fn failure_says_no_file_existed_when_none_did() {
        let fx = fixture();
        let prepared = PreparedSource::new(fx.ingress.clone()).unwrap();

        let err = discover_candidate(&prepared, &mut running_halt(), Duration::from_millis(350))
            .await
            .expect_err("nothing to attach");

        assert!(
            err.starts_with("no file named rollout-*.jsonl existed under"),
            "got: {err}"
        );
        let scans: u64 = err
            .split(" on any of ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("scan count missing from: {err}"));
        assert!(
            scans >= 3,
            "a 350ms window at a 100ms poll must scan several times; got {scans}: {err}"
        );
        assert!(err.contains("longest gap between scans"), "got: {err}");
    }

    /// A file rejected early in the window and then removed before the
    /// deadline must still be named in the failure — the final scan alone
    /// would see nothing and wrongly claim absence.
    #[tokio::test]
    async fn a_rejected_file_that_disappears_before_the_deadline_is_still_named() {
        let fx = fixture();
        let prepared = PreparedSource::new(fx.ingress.clone()).unwrap();
        let rollout = fx.root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl");
        std::fs::write(&rollout, session_meta_line("sess-1", &fx.other_dir)).unwrap();
        let to_remove = rollout.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = fs::remove_file(&to_remove);
        });

        let err = discover_candidate(&prepared, &mut running_halt(), Duration::from_millis(400))
            .await
            .expect_err("the file vanished before it ever correlated");

        assert!(
            err.contains("rollout-2026-09-13T21-41-42-sess-1.jsonl"),
            "a rejection seen earlier in the window must survive to the final message even though \
             the final scan no longer sees the file; got: {err}"
        );
        assert!(!err.starts_with("no file named"), "got: {err}");
    }

    /// A rollout that lands late in the window is attached by a later scan.
    #[tokio::test]
    async fn late_rollout_is_attached_before_the_deadline() {
        let fx = fixture();
        let prepared = PreparedSource::new(fx.ingress.clone()).unwrap();
        let rollout = fx.root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl");
        let line = session_meta_line("sess-1", &fx.workspace);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            fs::write(&rollout, line).unwrap();
        });

        let candidate = discover_candidate(&prepared, &mut running_halt(), Duration::from_secs(5))
            .await
            .unwrap()
            .expect("attached");
        assert_eq!(candidate.session_id, "sess-1");
    }

    /// A first line longer than the read cap is a permanent rejection with
    /// its own name, distinct from a line that is still being written.
    #[test]
    fn oversized_session_meta_is_a_permanent_rejection_and_a_short_one_is_transient() {
        let fx = fixture();
        let prepared = PreparedSource::new(fx.ingress.clone()).unwrap();

        let oversized = fx.root.join("rollout-big-sess-1.jsonl");
        let mut line = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"sess-1\",\"pad\":\"".to_owned();
        line.push_str(&"x".repeat(crate::agent_jsonl_progress::MAX_SESSION_META_BYTES as usize + 10));
        line.push_str("\"}}\n");
        fs::write(&oversized, line).unwrap();
        let verdict = validate_candidate_explained(&prepared, &oversized)
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(verdict, CandidateRejection::SessionMetaOversized { .. }),
            "got: {verdict:?}"
        );
        assert!(!verdict.is_transient());

        let partial = fx.root.join("rollout-partial-sess-2.jsonl");
        fs::write(&partial, "{\"type\":\"session_meta\"").unwrap();
        let verdict = validate_candidate_explained(&prepared, &partial).unwrap().unwrap_err();
        assert_eq!(verdict, CandidateRejection::SessionMetaUnterminated { bytes: 22 });
        assert!(verdict.is_transient());

        let empty = fx.root.join("rollout-empty-sess-3.jsonl");
        fs::write(&empty, "").unwrap();
        let verdict = validate_candidate_explained(&prepared, &empty).unwrap().unwrap_err();
        assert_eq!(verdict, CandidateRejection::Empty);
    }

    /// Each correlation check names itself.
    #[test]
    fn each_correlation_check_reports_its_own_reason() {
        let fx = fixture();
        let prepared = PreparedSource::new(fx.ingress.clone()).unwrap();

        let wrong_name = fx.root.join("rollout-wrong-name.jsonl");
        fs::write(&wrong_name, session_meta_line("sess-1", &fx.workspace)).unwrap();
        assert_eq!(
            validate_candidate_explained(&prepared, &wrong_name)
                .unwrap()
                .unwrap_err(),
            CandidateRejection::NameMismatch {
                expected_suffix: "-sess-1.jsonl".to_owned()
            }
        );

        let not_meta = fx.root.join("rollout-x-sess-2.jsonl");
        fs::write(&not_meta, "{\"type\":\"event_msg\",\"payload\":{}}\n").unwrap();
        assert_eq!(
            validate_candidate_explained(&prepared, &not_meta).unwrap().unwrap_err(),
            CandidateRejection::NotSessionMeta {
                record_type: Some("event_msg".to_owned())
            }
        );

        let no_cwd = fx.root.join("rollout-y-sess-3.jsonl");
        fs::write(&no_cwd, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"sess-3\"}}\n").unwrap();
        assert_eq!(
            validate_candidate_explained(&prepared, &no_cwd).unwrap().unwrap_err(),
            CandidateRejection::MissingCwd
        );

        let gone_cwd = fx.root.join("rollout-z-sess-4.jsonl");
        fs::write(
            &gone_cwd,
            session_meta_line("sess-4", &fx.other_dir.join("does-not-exist")),
        )
        .unwrap();
        assert!(matches!(
            validate_candidate_explained(&prepared, &gone_cwd).unwrap().unwrap_err(),
            CandidateRejection::CwdNotResolvable { .. }
        ));
    }

    /// The scan a reaper sees is the discovery scan: the same files, the
    /// same baseline, the same reasons.
    #[test]
    fn scan_partitions_matched_baseline_and_rejected() {
        let fx = fixture();
        let old = fx.root.join("rollout-old-sess-0.jsonl");
        fs::write(&old, session_meta_line("sess-0", &fx.workspace)).unwrap();
        let prepared = PreparedSource::new(fx.ingress.clone()).unwrap();
        assert_eq!(prepared.baseline.len(), 1, "the pre-existing rollout is baselined");

        fs::write(
            fx.root.join("rollout-new-sess-1.jsonl"),
            session_meta_line("sess-1", &fx.workspace),
        )
        .unwrap();
        fs::write(
            fx.root.join("rollout-foreign-sess-2.jsonl"),
            session_meta_line("sess-2", &fx.other_dir),
        )
        .unwrap();

        let scan = scan_once(&prepared).unwrap();
        assert_eq!(scan.matched.len(), 1);
        assert_eq!(scan.matched[0].session_id, "sess-1");
        let mut rejected = scan
            .rejected
            .iter()
            .map(|(path, why)| (path.file_name().unwrap().to_string_lossy().into_owned(), why.clone()))
            .collect::<Vec<_>>();
        rejected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(rejected[0].0, "rollout-foreign-sess-2.jsonl");
        assert!(matches!(rejected[0].1, CandidateRejection::CwdMismatch { .. }));
        assert_eq!(rejected[1].0, "rollout-old-sess-0.jsonl");
        assert_eq!(rejected[1].1, CandidateRejection::InBaseline);
    }

    /// A symlink whose name has the rollout shape must be surfaced as a
    /// rejection, not silently vanish from both `matched` and `rejected` —
    /// the shape that previously let an unreadable rollout read as `Absent`.
    #[cfg(unix)]
    #[test]
    fn name_shaped_symlink_is_reported_as_not_single_link_regular_file() {
        let fx = fixture();
        let prepared = PreparedSource::new(fx.ingress.clone()).unwrap();
        let real = fx.root.join("rollout-real-sess-1.jsonl");
        fs::write(&real, session_meta_line("sess-1", &fx.workspace)).unwrap();
        let link = fx.root.join("rollout-link-sess-2.jsonl");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let scan = scan_once(&prepared).unwrap();
        assert_eq!(scan.matched.len(), 1, "the real file still correlates");
        let symlink_rejection = scan
            .rejected
            .iter()
            .find(|(path, _)| path.file_name().unwrap().to_string_lossy() == "rollout-link-sess-2.jsonl");
        assert!(
            matches!(
                symlink_rejection,
                Some((_, CandidateRejection::NotSingleLinkRegularFile))
            ),
            "a name-shaped symlink must be rejected explicitly, not dropped; got: {scan:?}"
        );
    }

    /// A directory this process cannot read must fail the whole scan rather
    /// than silently skip it — an unreadable directory could be hiding the
    /// very rollout the reaper is asking about.
    #[cfg(unix)]
    #[test]
    fn unreadable_directory_fails_the_scan_rather_than_silently_dropping_it() {
        // Root ignores permission bits, so this guard is what keeps the test
        // honest instead of falsely passing under a root-run sandbox.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;

        let fx = fixture();
        // Built BEFORE the locked directory exists: `PreparedSource::new`'s
        // own baseline scan uses `scan_matching_paths` too, and would
        // otherwise fail here (correctly) rather than at the `scan_once`
        // call this test is actually about.
        let prepared = PreparedSource::new(fx.ingress.clone()).unwrap();
        let locked = fx.root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let result = scan_once(&prepared);

        // Restore permissions so the tempdir can be cleaned up.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.expect_err("an unreadable directory must fail the scan, not silently skip it");
        assert!(err.contains("locked") || err.contains("read"), "got: {err}");
    }

    // ─── the liveness probe ──────────────────────────────────────────────────

    #[derive(Default)]
    struct MapStore {
        checkpoints: Mutex<HashMap<String, IngressCheckpoint>>,
        fail_loads: bool,
    }

    impl IngressCheckpointStore for MapStore {
        fn store_ingress_checkpoint(&self, run_id: &str, checkpoint: &IngressCheckpoint) -> Result<(), String> {
            self.checkpoints
                .lock()
                .unwrap()
                .insert(run_id.to_owned(), checkpoint.clone());
            Ok(())
        }

        fn load_ingress_checkpoint(&self, run_id: &str) -> Result<Option<IngressCheckpoint>, String> {
            if self.fail_loads {
                return Err("simulated read failure".to_owned());
            }
            Ok(self.checkpoints.lock().unwrap().get(run_id).cloned())
        }
    }

    fn armed(fx: &Fixture, baseline: Vec<PathBuf>) -> IngressCheckpoint {
        IngressCheckpoint::Armed {
            ingress: fx.ingress.clone(),
            baseline,
        }
    }

    #[test]
    fn probe_reports_every_outcome_distinctly() {
        let fx = fixture();
        let store = MapStore::default();
        let now = 1_000_000;

        assert_eq!(
            probe_correlated_rollout(&store, "run-unknown", now),
            RolloutLiveness::NeverArmed
        );

        store
            .store_ingress_checkpoint("run-hooks", &IngressCheckpoint::NotFileIngress)
            .unwrap();
        assert_eq!(
            probe_correlated_rollout(&store, "run-hooks", now),
            RolloutLiveness::NotFileIngress
        );

        // Armed against an empty root: absent.
        store
            .store_ingress_checkpoint("run-1", &armed(&fx, Vec::new()))
            .unwrap();
        assert!(matches!(
            probe_correlated_rollout(&store, "run-1", now),
            RolloutLiveness::Absent { baseline_files: 0, .. }
        ));

        // A baselined rollout is not this run's.
        let old = fx.root.join("rollout-old-sess-0.jsonl");
        fs::write(&old, session_meta_line("sess-0", &fx.workspace)).unwrap();
        let old = fs::canonicalize(&old).unwrap();
        store.store_ingress_checkpoint("run-1", &armed(&fx, vec![old])).unwrap();
        assert!(matches!(
            probe_correlated_rollout(&store, "run-1", now),
            RolloutLiveness::Absent { baseline_files: 1, .. }
        ));

        // A new rollout — even one discovery would reject — is present.
        let foreign = fx.root.join("rollout-new-sess-1.jsonl");
        fs::write(&foreign, session_meta_line("sess-1", &fx.other_dir)).unwrap();
        match probe_correlated_rollout(&store, "run-1", now) {
            RolloutLiveness::Present {
                path,
                correlation,
                new_files,
                bytes,
                ..
            } => {
                assert_eq!(path, fs::canonicalize(&foreign).unwrap());
                assert!(matches!(correlation, Err(CandidateRejection::CwdMismatch { .. })));
                assert_eq!(new_files, 1);
                assert!(bytes > 0);
            }
            other => panic!("expected Present, got {other:?}"),
        }
        let summary = probe_correlated_rollout(&store, "run-1", now).to_string();
        assert!(summary.contains("discovery would NOT attach it"), "got: {summary}");

        // An unreadable store is undeterminable, never absent.
        let failing = MapStore {
            fail_loads: true,
            ..MapStore::default()
        };
        assert!(matches!(
            probe_correlated_rollout(&failing, "run-1", now),
            RolloutLiveness::Undeterminable(_)
        ));

        // A root that cannot be verified is undeterminable.
        let mut gone = fx.ingress.clone();
        gone.directory = fx.root.join("missing");
        store
            .store_ingress_checkpoint(
                "run-gone",
                &IngressCheckpoint::Armed {
                    ingress: gone,
                    baseline: Vec::new(),
                },
            )
            .unwrap();
        assert!(matches!(
            probe_correlated_rollout(&store, "run-gone", now),
            RolloutLiveness::Undeterminable(_)
        ));
    }
}
