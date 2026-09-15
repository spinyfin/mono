//! The liveness veto for never-started-spawn reaps.
//!
//! [`crate::spawn_ack_sweep`] reaps a slot when Boss has no
//! driver-originated signal for it. Before 2026-09-13 the only sources of
//! that signal were the hook ingress and the progress ingress; when the
//! progress ingress failed to attach a rollout that was on disk (see
//! [`crate::agent_jsonl_discovery`]), the run produced no signal, and the
//! sweep reaped four live workers as "driver never started" while their
//! transcripts were being appended to.
//!
//! This module asks the filesystem directly, immediately before a reap:
//! does a transcript for this execution exist? Two sources, both of which
//! only the driver itself can produce:
//!
//! 1. A rollout newer than the pre-spawn baseline under the run's watched
//!    root — [`crate::agent_jsonl_discovery::probe_correlated_rollout`],
//!    the same scan discovery runs, keyed off the run's durable ingress
//!    checkpoint.
//! 2. The transcript path recorded on the run row, which the hook ingress
//!    persists from the driver's own `transcript_path` payload.
//!
//! Either existing is a driver-originated signal in its own right: the
//! sweep records it as one (permanently, first-write-wins) and does not
//! reap. When neither can be established — a checkpoint that will not read,
//! a root that will not scan — the answer is
//! [`TranscriptLiveness::Undeterminable`], and a caller that would have
//! reaped must not: an unreadable answer is not "absent", and a false reap
//! destroys real work.
//!
//! [`probe_transcript_liveness`] is synchronous and blocking. Its caller
//! ([`crate::spawn_ack_sweep::reap_never_started_spawn`]) runs it inside
//! [`tokio::task::spawn_blocking`] rather than inline on the async reap
//! path, for the same reason discovery's own scan moved off the runtime.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::agent_jsonl_discovery::{RolloutLiveness, file_age_and_size, probe_correlated_rollout};
use crate::work::WorkDb;

/// Which surface produced the transcript evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptSource {
    /// A rollout under the run's progress-ingress root.
    Rollout,
    /// The `work_runs.transcript_path` the hook ingress recorded.
    RecordedTranscriptPath,
}

impl TranscriptSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TranscriptSource::Rollout => "rollout",
            TranscriptSource::RecordedTranscriptPath => "recorded_transcript_path",
        }
    }
}

/// The answer to "does a transcript for this execution exist on disk?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptLiveness {
    /// A transcript exists. `age_secs` is seconds since its last write.
    Present {
        source: TranscriptSource,
        path: PathBuf,
        age_secs: i64,
        bytes: u64,
        /// Whether discovery's own correlation rules would attach this file
        /// to the run. Always `true` for [`TranscriptSource::RecordedTranscriptPath`]
        /// (the hook ingress already correlated it). For
        /// [`TranscriptSource::Rollout`] this mirrors
        /// [`crate::agent_jsonl_discovery::RolloutLiveness::Present`]'s
        /// `correlation`: `false` means the file is the driver's but
        /// discovery would reject it (a cwd mismatch, an oversized
        /// `session_meta`, etc.) — proof the driver ran, but a distinct
        /// signal from a clean attachment, worth telling apart at the reap.
        discovery_would_attach: bool,
        /// The probe's own description of what it found, for the log.
        detail: String,
    },
    /// Every source was consulted and none has a transcript. `checked`
    /// lists what was looked at, so the reap narrative can say so.
    Absent { checked: Vec<String> },
    /// At least one source could not be consulted, and no source proved a
    /// transcript. `reasons` says what could not be read.
    Undeterminable { reasons: Vec<String>, checked: Vec<String> },
}

impl TranscriptLiveness {
    /// Whether a reap must not proceed. `true` for a present transcript
    /// (the driver demonstrably ran) and for an undeterminable answer (the
    /// question could not be asked, which is not the same as "no").
    pub fn vetoes_reap(&self) -> bool {
        !matches!(self, TranscriptLiveness::Absent { .. })
    }

    /// Whether a transcript for this execution was found on disk.
    pub fn is_present(&self) -> bool {
        matches!(self, TranscriptLiveness::Present { .. })
    }
}

impl fmt::Display for TranscriptLiveness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TranscriptLiveness::Present { source, detail, .. } => {
                write!(f, "transcript present ({}): {detail}", source.as_str())
            }
            TranscriptLiveness::Absent { checked } => {
                write!(f, "no transcript exists: {}", checked.join("; "))
            }
            TranscriptLiveness::Undeterminable { reasons, checked } => {
                write!(f, "liveness could not be established: {}", reasons.join("; "))?;
                if !checked.is_empty() {
                    write!(f, " (also checked: {})", checked.join("; "))?;
                }
                Ok(())
            }
        }
    }
}

/// Consult every transcript source for `execution_id`.
///
/// Synchronous and blocking (one DB read per source plus a directory scan);
/// call it from [`tokio::task::spawn_blocking`] rather than inline on an
/// async task — the directory scan alone can walk up to `MAX_DISCOVERY_DIRS`
/// directories, which is exactly the kind of unbounded-latency filesystem
/// work this repo's discovery rework moved off the runtime for the same
/// reason (see `crate::agent_jsonl_discovery`'s module doc).
///
/// `spawned_at_epoch_secs` scopes the [`TranscriptSource::RecordedTranscriptPath`]
/// source to this run's own incarnation: a transcript path resolved through
/// `work_db.transcript_path_for_execution` can belong to an earlier
/// incarnation of the same execution row (the resolver actively prefers
/// whichever run row has a non-NULL path), so a file last written before
/// this spawn is reported in `checked`, not treated as proof. The
/// [`TranscriptSource::Rollout`] source needs no such bound: its baseline is
/// taken at this spawn, so anything it finds already post-dates it.
pub fn probe_transcript_liveness(
    work_db: &WorkDb,
    execution_id: &str,
    now_epoch_secs: i64,
    spawned_at_epoch_secs: Option<i64>,
) -> TranscriptLiveness {
    let mut checked = Vec::new();
    let mut reasons = Vec::new();

    match probe_correlated_rollout(work_db, execution_id, now_epoch_secs) {
        RolloutLiveness::Present {
            path,
            age_secs,
            bytes,
            correlation,
            new_files,
        } => {
            let detail = RolloutLiveness::Present {
                path: path.clone(),
                age_secs,
                bytes,
                correlation: correlation.clone(),
                new_files,
            }
            .to_string();
            return TranscriptLiveness::Present {
                source: TranscriptSource::Rollout,
                path,
                age_secs,
                bytes,
                discovery_would_attach: correlation.is_ok(),
                detail,
            };
        }
        RolloutLiveness::Undeterminable(reason) => reasons.push(format!("rollout: {reason}")),
        other @ (RolloutLiveness::Absent { .. } | RolloutLiveness::NotFileIngress | RolloutLiveness::NeverArmed) => {
            checked.push(format!("rollout: {other}"));
        }
    }

    match work_db.transcript_path_for_execution(execution_id) {
        Ok(Some(recorded)) => match recorded_transcript(Path::new(&recorded), now_epoch_secs) {
            Ok(Some((age_secs, bytes))) => {
                let modified_epoch = now_epoch_secs.saturating_sub(age_secs);
                let predates_this_spawn = spawned_at_epoch_secs.is_some_and(|spawned_at| modified_epoch < spawned_at);
                if predates_this_spawn {
                    checked.push(format!(
                        "recorded transcript path {recorded} exists but was last written {age_secs}s ago, \
                         before this run's spawn — likely left by an earlier incarnation of this execution"
                    ));
                } else {
                    return TranscriptLiveness::Present {
                        source: TranscriptSource::RecordedTranscriptPath,
                        detail: format!(
                            "recorded transcript path {recorded} exists ({bytes} bytes, last written {age_secs}s ago)"
                        ),
                        path: PathBuf::from(recorded),
                        age_secs,
                        bytes,
                        discovery_would_attach: true,
                    };
                }
            }
            Ok(None) => checked.push(format!("recorded transcript path {recorded} does not exist")),
            Err(err) => reasons.push(format!("recorded transcript path {recorded}: {err}")),
        },
        Ok(None) => checked.push("no transcript path is recorded on the run row".to_owned()),
        Err(err) => reasons.push(format!("the run row's transcript path could not be read: {err:#}")),
    }

    if reasons.is_empty() {
        TranscriptLiveness::Absent { checked }
    } else {
        TranscriptLiveness::Undeterminable { reasons, checked }
    }
}

/// `Ok(Some((age, bytes)))` when the recorded path is a file, `Ok(None)`
/// when it does not exist, `Err` for anything else.
fn recorded_transcript(path: &Path, now_epoch_secs: i64) -> Result<Option<(i64, u64)>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => file_age_and_size(path, now_epoch_secs).map(Some),
        Ok(metadata) => Err(format!("exists but is not a regular file ({:?})", metadata.file_type())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("stat failed: {err}")),
    }
}
