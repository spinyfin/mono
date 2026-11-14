//! Hub custody of the transcript and artifact bytes a spoke journals.
//!
//! A spoke streams a run's transcript, its registered artifacts (structured
//! output, PR-url output, attachments) and gaps in what it could read as records
//! in its journal. The hub keeps those records as received; this crate turns
//! them into files the engine's existing readers can open, keyed by run identity
//! rather than by any path on the host.
//!
//! * **Offset- and digest-safe.** A chunk is written at its own offset and
//!   fsynced before the ledger (`spoke_custody`) records it. Replaying a chunk
//!   compares the bytes already held with the chunk and writes nothing; a hole
//!   or a changed overlap halts that one resource and never fabricates a
//!   complete file. See [`write`].
//! * **Honest absence.** What a reader gets back says *why* bytes are missing:
//!   still expected ([`Evidence::Pending`], [`OutputEvidence::Incomplete`]),
//!   declared unavailable by the spoke or the hub ([`Evidence::Unavailable`]),
//!   or positively bad ([`OutputEvidence::Invalid`]). The first is never
//!   reported as the second.
//! * **Recorded roots.** Every file lives under `<root>/<execution>/spoke/<run>`
//!   and the ledger records each path. Retention deletes those recorded files
//!   (and empty directories they leave), never extra files a directory scan
//!   might find beside them.
//!
//! Nothing here decides what evidence means for work status.

mod write {
    //! Exact-offset, replay-safe file writes.
    //!
    //! The one rule that makes materialization safe to repeat: a chunk is applied
    //! at its own offset, never appended. Bytes the hub already holds are compared
    //! with the replayed chunk instead of written again, so a crash between the
    //! write and the ledger row, a duplicated delivery and a restart all converge on
    //! the same file.

    use std::fs::{DirBuilder, File, OpenOptions};
    use std::io::{self, Read};
    use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
    use std::path::Path;

    use sha2::{Digest, Sha256};

    /// What applying one chunk did to a file.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Extended {
        /// The file now holds this many contiguous bytes, all durable.
        Wrote(u64),
        /// Every byte of the chunk was already held, and is identical.
        Replayed,
        /// The chunk starts past the end of what is held; the bytes between are
        /// missing and nothing here can supply them.
        Hole { expected: u64 },
        /// The hub's own copy disagrees with the chunk or with its ledger.
        Changed(String),
    }

    /// Apply `data` at `offset` of the file at `path`, given that the ledger says
    /// `materialized` contiguous bytes are already durable. The file is fsynced
    /// (and its directory, when it was created) before `Wrote` is returned, so the
    /// caller may then advance the ledger.
    pub(crate) fn extend(path: &Path, materialized: u64, offset: u64, data: &[u8]) -> io::Result<Extended> {
        if offset > materialized {
            return Ok(Extended::Hole { expected: materialized });
        }
        let end = offset + data.len() as u64;
        let parent = path.parent();
        if let Some(parent) = parent {
            DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        }
        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        let held = file.metadata()?.len();
        if held < materialized {
            return Ok(Extended::Changed(format!(
                "the hub's copy holds {held} bytes but {materialized} were recorded as written"
            )));
        }
        let overlap = (end.min(materialized) - offset) as usize;
        if overlap > 0 {
            let mut stored = vec![0u8; overlap];
            file.read_exact_at(&mut stored, offset)?;
            if Sha256::digest(&stored) != Sha256::digest(&data[..overlap]) {
                return Ok(Extended::Changed(format!(
                    "the {overlap} bytes already held at offset {offset} differ from the replayed chunk"
                )));
            }
        }
        if end <= materialized {
            return Ok(Extended::Replayed);
        }
        file.write_all_at(&data[overlap..], materialized)?;
        // Whatever a crashed earlier write left past this chunk is not evidence.
        file.set_len(end)?;
        file.sync_all()?;
        if !existed && let Some(parent) = parent {
            File::open(parent)?.sync_all()?;
        }
        Ok(Extended::Wrote(end))
    }

    /// Lowercase hex SHA-256 of the first `len` bytes of `path`, or why they cannot
    /// be read as exactly that many.
    pub(crate) fn digest_hex(path: &Path, len: u64) -> Result<String, String> {
        let mut file = File::open(path).map_err(|error| format!("the hub's copy cannot be read: {error}"))?;
        let held = file.metadata().map_err(|error| error.to_string())?.len();
        if held != len {
            return Err(format!("the hub's copy holds {held} bytes, expected {len}"));
        }
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
    }

    /// The last `max_bytes` of the first `materialized` bytes of the file,
    /// lossily decoded. Bytes a write left past the ledger are not read.
    pub(crate) fn tail(path: &Path, materialized: u64, max_bytes: u64) -> io::Result<String> {
        let file = File::open(path)?;
        let held = file.metadata()?.len();
        let len = materialized.min(held);
        let start = len.saturating_sub(max_bytes);
        let mut bytes = vec![0u8; (len - start) as usize];
        file.read_exact_at(&mut bytes, start)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;

        fn file() -> (tempfile::TempDir, std::path::PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a").join("b.bin");
            (dir, path)
        }

        #[test]
        fn a_replay_writes_nothing_and_a_longer_replay_writes_only_the_new_tail() {
            let (_dir, path) = file();
            assert_eq!(extend(&path, 0, 0, b"hello").unwrap(), Extended::Wrote(5));
            assert_eq!(extend(&path, 5, 0, b"hello").unwrap(), Extended::Replayed);
            assert_eq!(extend(&path, 5, 3, b"lo wor").unwrap(), Extended::Wrote(9));
            assert_eq!(fs::read(&path).unwrap(), b"hello wor");
        }

        #[test]
        fn a_crashed_write_past_the_ledger_is_overwritten_not_appended() {
            let (_dir, path) = file();
            assert_eq!(extend(&path, 0, 0, b"abc").unwrap(), Extended::Wrote(3));
            // A write that reached the disk but not the ledger.
            fs::write(&path, b"abcSTALE").unwrap();
            assert_eq!(extend(&path, 3, 3, b"def").unwrap(), Extended::Wrote(6));
            assert_eq!(fs::read(&path).unwrap(), b"abcdef");
        }

        #[test]
        fn holes_changed_overlaps_and_shortened_files_are_refused() {
            let (_dir, path) = file();
            extend(&path, 0, 0, b"abcd").unwrap();
            assert_eq!(extend(&path, 4, 9, b"x").unwrap(), Extended::Hole { expected: 4 });
            assert!(matches!(extend(&path, 4, 2, b"ZZ").unwrap(), Extended::Changed(_)));
            fs::write(&path, b"ab").unwrap();
            assert!(matches!(extend(&path, 4, 4, b"e").unwrap(), Extended::Changed(_)));
        }
    }
}

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use boss_engine_structured_output::PrUrlPayload;
use boss_protocol_spoke::v1::{ArtifactChunk, ArtifactKind, Gap, Run, TranscriptDelta};
use boss_work_db::WorkDb;
use sha2::{Digest, Sha256};
use write::Extended;

pub use boss_engine_structured_output::StructuredOutputKind;
pub use boss_work_db::{CustodyState, SpokeCustody};

/// The transcript resource a gap with no generation names: the spoke could not
/// say which transcript, or that it ever attached one.
pub const BARE_TRANSCRIPT: &str = "transcript";
/// A worker output the spoke found and refused: positively invalid.
const OUTPUT_PREFIX: &str = "output:";
/// The spool could not take a worker output: unavailable, not invalid.
const OUTPUT_CAPTURE: &str = "output:capture";
/// The spoke finished capturing outputs for a boundary. `reason` is a JSON
/// array of artifact identities registered for that capture; an empty array
/// means the capture found nothing to send.
const OUTPUT_CONCLUDED: &str = "output:concluded";
const PR_URL_OUTPUT: &str = "output:BOSS_PR_URL_OUTPUT";
const STRUCTURED_OUTPUT: &str = "output:BOSS_STRUCTURED_OUTPUT";
const MAX_RESOURCE_CHARS: usize = 256;
const MAX_REASON_CHARS: usize = 1024;
const OPEN_RETRIES: usize = 4;

/// Size bounds that protect the hub's disk from a peer's claims.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_transcript_bytes: u64,
    pub max_artifact_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_transcript_bytes: 512 * 1024 * 1024,
            max_artifact_bytes: boss_protocol::ATTACHMENT_MAX_BYTES,
        }
    }
}

/// Who a record came from.
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    pub host_id: &'a str,
    pub run: &'a Run,
    /// The ingress sequence of the record.
    pub sequence: u64,
}

/// What applying one record did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// New bytes are durably on the hub. `first` when this created the resource.
    Wrote {
        resource: String,
        materialized: u64,
        first: bool,
    },
    /// Bytes the hub already holds, verified identical.
    Replayed { resource: String },
    /// A state change with no bytes: a gap, a closed resource, a verified artifact.
    Recorded,
    /// Nothing to do, and why.
    Ignored(&'static str),
    /// The hub cannot trust this resource and will not project it further. A
    /// halted resource reports this on every later record, so the alert is
    /// raised from whichever record reaches the projector first.
    Halted { resource: String, reason: String },
}

/// The transcript a completion boundary needs: a generation and how far into it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mark {
    pub generation: String,
    pub through: u64,
    pub capture_epoch: u64,
}

impl Mark {
    pub fn new(generation: &str, through: u64) -> Self {
        Self {
            generation: generation.to_owned(),
            through,
            capture_epoch: 0,
        }
    }
}

/// A transcript file on the hub, addressed by run identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSource {
    pub path: PathBuf,
    /// The run's custody directory: the containment root readers confine the
    /// path to.
    pub root: PathBuf,
    pub generation: String,
    pub materialized: u64,
}

/// How far a transcript watermark has been met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingCause {
    Transcript,
    OutputsNotConcluded,
    Artifact { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// Every byte through the mark is on the hub.
    Ready(TranscriptSource),
    /// The bytes are still expected.
    Pending { have: u64, want: u64, cause: PendingCause },
    /// They will not come, and why.
    Unavailable(String),
}

/// What the hub holds of a worker's structured output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputEvidence {
    /// Whole, digest-checked and well formed.
    Ready { path: PathBuf, bytes: u64 },
    /// Registered and still arriving.
    Incomplete { have: u64, total: u64 },
    /// Positive evidence the output cannot be trusted.
    Invalid(String),
    /// Will not arrive, or the hub could not keep it. Not evidence about the
    /// output itself.
    Unavailable(String),
    /// Nothing registered.
    Absent,
}

/// How collecting an output into its destination ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collected {
    Installed,
    NotAvailable(String),
    Failed(String),
}

/// The custody directory and its ledger. Cheap to clone.
#[derive(Clone)]
pub struct Custody {
    db: WorkDb,
    root: PathBuf,
    limits: Limits,
}

impl Custody {
    pub fn new(db: WorkDb, root: impl Into<PathBuf>) -> Self {
        Self {
            db,
            root: root.into(),
            limits: Limits::default(),
        }
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding one run's files; the containment root for readers.
    pub fn run_dir(&self, execution_id: &str, run_id: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(segment(execution_id)?)
            .join("spoke")
            .join(segment(run_id)?))
    }

    fn relative(&self, run: &Run, prefix: &str, resource: &str, ext: &str) -> Result<String> {
        let digest = Sha256::digest(resource.as_bytes());
        let name: String = digest[..8].iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(format!(
            "{}/spoke/{}/{prefix}-{name}.{ext}",
            segment(&run.execution_id)?,
            segment(&run.run_id)?
        ))
    }

    // ── Writing ─────────────────────────────────────────────────────────────

    pub fn apply_transcript(&self, source: &Source<'_>, delta: &TranscriptDelta) -> Result<Applied> {
        let resource = format!("transcript:{}", delta.generation);
        if let Some(halted) = self.screen(source, "transcript", &resource, &delta.data, &delta.sha256)? {
            return Ok(halted);
        }
        let relative = self.relative(source.run, "transcript", &resource, "jsonl")?;
        let spec = Spec {
            kind: "transcript",
            resource,
            relative,
            limit: self.limits.max_transcript_bytes,
            artifact: None,
        };
        self.extend_resource(source, &spec, delta.offset, &delta.data, delta.eof)
    }

    pub fn apply_artifact(&self, source: &Source<'_>, chunk: &ArtifactChunk) -> Result<Applied> {
        let resource = format!("artifact:{}", chunk.artifact_id);
        if let Some(halted) = self.screen(source, "artifact", &resource, &chunk.data, &chunk.sha256)? {
            return Ok(halted);
        }
        let digest = match identity_digest(&chunk.artifact_id, chunk.kind) {
            Ok(digest) => digest,
            Err(reason) => {
                return self.close(source, "artifact", &resource, CustodyState::Invalid, &reason, 0);
            }
        };
        let relative = self.relative(source.run, "artifact", &resource, "bin")?;
        let spec = Spec {
            kind: "artifact",
            resource: resource.clone(),
            relative,
            limit: self.limits.max_artifact_bytes,
            artifact: Some(ArtifactSpec {
                kind: chunk.kind,
                total: chunk.total_bytes,
                digest,
                metadata: String::from_utf8_lossy(&chunk.metadata_json).into_owned(),
            }),
        };
        if chunk.total_bytes == 0 {
            return self.halt(
                source,
                "artifact",
                &resource,
                "an artifact chunk declares no total size",
            );
        }
        self.extend_resource(source, &spec, chunk.offset, &chunk.data, chunk.eof)
    }

    /// Record what a gap says about a resource. Informational gaps change nothing.
    pub fn apply_gap(&self, source: &Source<'_>, gap: &Gap) -> Result<Applied> {
        let reason = bounded(&gap.reason);
        let resource = gap.resource.as_str();
        if resource.chars().count() > MAX_RESOURCE_CHARS {
            return Ok(Applied::Ignored("gap resource name too long"));
        }
        if resource == BARE_TRANSCRIPT {
            return self.close(
                source,
                "transcript",
                BARE_TRANSCRIPT,
                CustodyState::Unavailable,
                &reason,
                0,
            );
        }
        if let Some(rest) = resource.strip_prefix("transcript:") {
            if rest.contains(':') {
                // Progress normalization skipped an oversized line; the raw
                // bytes were kept, so the transcript itself is intact.
                return Ok(Applied::Ignored("informational transcript gap"));
            }
            return self.close(
                source,
                "transcript",
                resource,
                CustodyState::Unavailable,
                &reason,
                gap.from_offset,
            );
        }
        if resource == OUTPUT_CAPTURE {
            return self.close(source, "output", resource, CustodyState::Unavailable, &reason, 0);
        }
        if resource == OUTPUT_CONCLUDED
            || resource
                .strip_prefix("output:concluded:")
                .is_some_and(|epoch| epoch.parse::<u64>().is_ok())
        {
            return self.close(source, "output", resource, CustodyState::Complete, &reason, 0);
        }
        if resource == PR_URL_OUTPUT || resource == STRUCTURED_OUTPUT {
            return self.close(source, "output", resource, CustodyState::Invalid, &reason, 0);
        }
        if resource.starts_with(OUTPUT_PREFIX) {
            return Ok(Applied::Ignored("unrecognised output gap"));
        }
        if let Some(rest) = resource.strip_prefix("artifact:")
            && !rest.is_empty()
        {
            return self.close(
                source,
                "artifact",
                resource,
                CustodyState::Unavailable,
                &reason,
                gap.from_offset,
            );
        }
        Ok(Applied::Ignored("gap about a resource custody does not hold"))
    }

    /// Reject a chunk whose bytes do not match its own digest, or a resource
    /// name no ledger row should carry.
    fn screen(
        &self,
        source: &Source<'_>,
        kind: &'static str,
        resource: &str,
        data: &[u8],
        digest: &[u8],
    ) -> Result<Option<Applied>> {
        if resource.chars().count() > MAX_RESOURCE_CHARS {
            return self
                .halt(source, kind, &bounded(resource), "the resource name is too long")
                .map(Some);
        }
        if Sha256::digest(data)[..] != *digest {
            return self
                .halt(source, kind, resource, "a chunk's bytes do not match its digest")
                .map(Some);
        }
        Ok(None)
    }

    fn extend_resource(
        &self,
        source: &Source<'_>,
        spec: &Spec,
        offset: u64,
        data: &[u8],
        eof: bool,
    ) -> Result<Applied> {
        if data.is_empty() {
            return Ok(Applied::Ignored("an empty chunk carries no bytes"));
        }
        let run = source.run;
        let end = offset + data.len() as u64;
        for _ in 0..OPEN_RETRIES {
            let current = self
                .db
                .spoke_custody(&run.execution_id, run.assignment_generation, &spec.resource)?;
            if let Some(row) = &current {
                match row.state {
                    CustodyState::Halted => return Ok(halted(&spec.resource, row)),
                    CustodyState::Open => {}
                    _ if end <= row.materialized => {
                        return Ok(Applied::Replayed {
                            resource: spec.resource.clone(),
                        });
                    }
                    _ => return Ok(Applied::Ignored("the resource is closed")),
                }
                if let Some(artifact) = &spec.artifact
                    && (row.total_bytes != Some(artifact.total) || row.artifact_kind != Some(artifact.kind))
                {
                    return self.halt(
                        source,
                        spec.kind,
                        &spec.resource,
                        "an artifact chunk disagrees with the size or kind its earlier chunks declared",
                    );
                }
            }
            if end > spec.limit {
                return self.halt(
                    source,
                    spec.kind,
                    &spec.resource,
                    &format!("the resource would pass the hub's {} byte custody limit", spec.limit),
                );
            }
            let materialized = current.as_ref().map_or(0, |row| row.materialized);
            let path = self.root.join(&spec.relative);
            match write::extend(&path, materialized, offset, data)
                .with_context(|| format!("writing custody file {}", path.display()))?
            {
                Extended::Hole { expected } => {
                    return self.halt(
                        source,
                        spec.kind,
                        &spec.resource,
                        &format!("expected bytes from offset {expected} but the record starts at {offset}"),
                    );
                }
                Extended::Changed(why) => return self.halt(source, spec.kind, &spec.resource, &why),
                Extended::Replayed => {
                    return Ok(Applied::Replayed {
                        resource: spec.resource.clone(),
                    });
                }
                Extended::Wrote(now) => {
                    let mut next = self.base_row(source, spec.kind, &spec.resource);
                    next.path = spec.relative.clone();
                    next.materialized = now;
                    next.caught_up = eof;
                    if let Some(artifact) = &spec.artifact {
                        next.total_bytes = Some(artifact.total);
                        next.artifact_kind = Some(artifact.kind);
                        next.expected_digest = Some(artifact.digest.clone());
                        next.metadata_json = Some(artifact.metadata.clone());
                        if now > artifact.total {
                            return self.halt(
                                source,
                                spec.kind,
                                &spec.resource,
                                &format!("the stream wrote {now} bytes, past the {} it declared", artifact.total),
                            );
                        }
                        if eof && now != artifact.total {
                            return self.halt(
                                source,
                                spec.kind,
                                &spec.resource,
                                &format!("the stream ended at {now} of {} declared bytes", artifact.total),
                            );
                        }
                        if now == artifact.total {
                            match verified(&path, artifact) {
                                Ok(()) => next.state = CustodyState::Complete,
                                Err(reason) => {
                                    next.state = CustodyState::Invalid;
                                    next.reason = Some(reason);
                                }
                            }
                        }
                    }
                    let expected = current.as_ref().map(|row| (row.materialized, row.state));
                    if self.db.put_spoke_custody(&next, expected)? {
                        return Ok(Applied::Wrote {
                            resource: spec.resource.clone(),
                            materialized: now,
                            first: current.is_none(),
                        });
                    }
                    // Another writer moved the row first; read it again.
                }
            }
        }
        Err(anyhow!(
            "custody row for {} kept changing under the writer",
            spec.resource
        ))
    }

    /// Stop projecting one resource: the hub's copy cannot be trusted.
    fn halt(&self, source: &Source<'_>, kind: &'static str, resource: &str, reason: &str) -> Result<Applied> {
        let reason = bounded(reason);
        let run = source.run;
        let existing = self
            .db
            .spoke_custody(&run.execution_id, run.assignment_generation, resource)?;
        if let Some(row) = &existing
            && row.state == CustodyState::Halted
        {
            return Ok(halted(resource, row));
        }
        let mut next = existing
            .clone()
            .unwrap_or_else(|| self.base_row(source, kind, resource));
        next.state = CustodyState::Halted;
        next.reason = Some(reason.clone());
        next.source_sequence = source.sequence;
        let expected = existing.as_ref().map(|row| (row.materialized, row.state));
        // A lost race means another writer already recorded a verdict; the
        // next record reports whatever it was.
        let _ = self.db.put_spoke_custody(&next, expected)?;
        Ok(Applied::Halted {
            resource: resource.to_owned(),
            reason,
        })
    }

    /// Move a resource to a closing state (`unavailable` or `invalid`), keeping
    /// the bytes already held. A hole in front of the gap halts it instead.
    fn close(
        &self,
        source: &Source<'_>,
        kind: &'static str,
        resource: &str,
        state: CustodyState,
        reason: &str,
        from_offset: u64,
    ) -> Result<Applied> {
        let run = source.run;
        for _ in 0..OPEN_RETRIES {
            let existing = self
                .db
                .spoke_custody(&run.execution_id, run.assignment_generation, resource)?;
            let mut next = match &existing {
                Some(row) if row.state == CustodyState::Halted => return Ok(halted(resource, row)),
                Some(row) if row.state != CustodyState::Open && state == CustodyState::Unavailable => {
                    return Ok(Applied::Ignored("the resource is already closed"));
                }
                Some(row) => row.clone(),
                None => self.base_row(source, kind, resource),
            };
            if from_offset > next.materialized {
                return self.halt(
                    source,
                    kind,
                    resource,
                    &format!(
                        "the spoke reports a gap after {from_offset} bytes but the hub holds {}",
                        next.materialized
                    ),
                );
            }
            next.state = state;
            next.reason = Some(reason.to_owned());
            next.source_sequence = source.sequence;
            let expected = existing.as_ref().map(|row| (row.materialized, row.state));
            if self.db.put_spoke_custody(&next, expected)? {
                return Ok(Applied::Recorded);
            }
        }
        Err(anyhow!("custody row for {resource} kept changing under the writer"))
    }

    fn base_row(&self, source: &Source<'_>, kind: &'static str, resource: &str) -> SpokeCustody {
        SpokeCustody::builder()
            .execution_id(&source.run.execution_id)
            .assignment_generation(source.run.assignment_generation)
            .resource(resource)
            .host_id(source.host_id)
            .run_id(&source.run.run_id)
            .kind(kind)
            .state(CustodyState::Open)
            .source_sequence(source.sequence)
            .build()
    }

    // ── Reading transcripts ─────────────────────────────────────────────────

    /// Whether `mark` has been met for the run, and if not, whether it will be.
    ///
    /// When `artifacts_settled`, a completion boundary also waits for the
    /// spoke's capture-concluded marker and for every artifact identity that
    /// marker names. A named artifact with no row is pending, not settled.
    /// Absence of open artifact rows is not treated as settled.
    pub fn transcript_evidence(&self, run: &Run, mark: &Mark, artifacts_settled: bool) -> Result<Evidence> {
        let rows = self.db.spoke_custody_for_execution(&run.execution_id)?;
        let mine: Vec<&SpokeCustody> = rows
            .iter()
            .filter(|row| row.assignment_generation == run.assignment_generation)
            .collect();
        if artifacts_settled && let Some(pending) = self.capture_pending(&mine, mark.capture_epoch) {
            return Ok(pending);
        }
        self.transcript_mark(&mine, mark)
    }

    /// Capture has not finished, or a named artifact is still arriving.
    fn capture_pending(&self, rows: &[&SpokeCustody], epoch: u64) -> Option<Evidence> {
        let capture_fail = epoch == 0 && rows.iter().any(|row| row.resource == OUTPUT_CAPTURE);
        let resource = if epoch == 0 {
            OUTPUT_CONCLUDED.to_owned()
        } else {
            format!("{OUTPUT_CONCLUDED}:{epoch}")
        };
        let concluded = rows
            .iter()
            .filter(|row| {
                row.resource == resource
                    || (epoch > 0
                        && row
                            .resource
                            .strip_prefix("output:concluded:")
                            .and_then(|n| n.parse::<u64>().ok())
                            .is_some_and(|n| n >= epoch))
            })
            .min_by_key(|row| row.source_sequence);
        if concluded.is_none() && !capture_fail {
            return Some(Evidence::Pending {
                have: 0,
                want: 1,
                cause: PendingCause::OutputsNotConcluded,
            });
        }
        let concluded = concluded?;
        let ids: Vec<String> = serde_json::from_str(concluded.reason.as_deref().unwrap_or("[]")).unwrap_or_default();
        for id in ids {
            let resource = format!("artifact:{id}");
            match rows.iter().find(|row| row.resource == resource) {
                None => {
                    return Some(Evidence::Pending {
                        have: 0,
                        want: 1,
                        cause: PendingCause::Artifact { id },
                    });
                }
                Some(row) if row.state == CustodyState::Open => {
                    return Some(Evidence::Pending {
                        have: row.materialized,
                        want: row.total_bytes.unwrap_or(row.materialized),
                        cause: PendingCause::Artifact { id },
                    });
                }
                Some(_) => {}
            }
        }
        None
    }

    fn transcript_mark(&self, rows: &[&SpokeCustody], mark: &Mark) -> Result<Evidence> {
        if mark.generation.is_empty() {
            let reason = rows
                .iter()
                .find(|row| row.resource == BARE_TRANSCRIPT)
                .and_then(|row| row.reason.clone())
                .unwrap_or_else(|| "the spoke reported no transcript watermark".to_owned());
            return Ok(Evidence::Unavailable(reason));
        }
        let resource = format!("transcript:{}", mark.generation);
        let Some(row) = rows.iter().find(|row| row.resource == resource) else {
            return Ok(if mark.through == 0 {
                Evidence::Unavailable("the transcript is empty at this boundary".to_owned())
            } else {
                Evidence::Pending {
                    have: 0,
                    want: mark.through,
                    cause: PendingCause::Transcript,
                }
            });
        };
        Ok(match row.state {
            _ if row.path.is_empty() && row.state != CustodyState::Halted => Evidence::Unavailable(format!(
                "the transcript holds no bytes: {}",
                row.reason.as_deref().unwrap_or("closed")
            )),
            CustodyState::Halted => Evidence::Unavailable(format!(
                "the hub's copy was halted: {}",
                row.reason.as_deref().unwrap_or("integrity failure")
            )),
            _ if row.materialized >= mark.through => Evidence::Ready(self.source_of(row)?),
            CustodyState::Open => Evidence::Pending {
                have: row.materialized,
                want: mark.through,
                cause: PendingCause::Transcript,
            },
            _ => Evidence::Unavailable(format!(
                "the transcript ends at {} bytes, before the {} the boundary needs: {}",
                row.materialized,
                mark.through,
                row.reason.as_deref().unwrap_or("closed")
            )),
        })
    }

    /// The hub copy of one transcript generation of a run, once it holds bytes.
    pub fn transcript_source(&self, run: &Run, generation: &str) -> Result<Option<TranscriptSource>> {
        let resource = format!("transcript:{generation}");
        self.db
            .spoke_custody(&run.execution_id, run.assignment_generation, &resource)?
            .filter(|row| !row.path.is_empty())
            .map(|row| self.source_of(&row))
            .transpose()
    }

    /// The execution's newest transcript generation that holds bytes.
    ///
    /// Used by adapter tests to inspect generation replacement.
    pub fn latest_transcript(&self, execution_id: &str) -> Result<Option<(TranscriptSource, CustodyState)>> {
        let rows = self.db.spoke_custody_for_execution(execution_id)?;
        rows.iter()
            .find(|row| row.kind == "transcript" && !row.path.is_empty() && row.materialized > 0)
            .map(|row| Ok((self.source_of(row)?, row.state)))
            .transpose()
    }

    fn source_of(&self, row: &SpokeCustody) -> Result<TranscriptSource> {
        Ok(TranscriptSource {
            path: self.root.join(&row.path),
            root: self.run_dir(&row.execution_id, &row.run_id)?,
            generation: row.resource.trim_start_matches("transcript:").to_owned(),
            materialized: row.materialized,
        })
    }

    /// The last `max_bytes` of the file at `path` if the ledger recorded it as
    /// one of ours, and `None` for any other path: custody never opens a path it
    /// did not write, least of all one that names a file on a spoke's host.
    /// Halted or invalid rows are an error, not a silent miss, so a caller
    /// cannot fall through to another path. The read is bounded to the
    /// committed watermark.
    pub fn read_tail_at(&self, path: &Path, max_bytes: u64) -> Result<Option<String>> {
        let Some(limit) = self.read_limit_at(path)? else {
            return Ok(None);
        };
        write::tail(path, limit, max_bytes)
            .map(Some)
            .with_context(|| format!("reading {}", path.display()))
    }

    /// Validate the ledger state and return the committed byte limit.
    pub fn read_limit_at(&self, path: &Path) -> Result<Option<u64>> {
        let Some(relative) = path.strip_prefix(&self.root).ok().and_then(Path::to_str) else {
            return Ok(None);
        };
        let Some(row) = self.db.spoke_custody_at(relative)? else {
            return Ok(None);
        };
        match row.state {
            CustodyState::Halted | CustodyState::Invalid => {
                anyhow::bail!(
                    "the hub's copy of this transcript is {}: {}",
                    row.state.as_str(),
                    row.reason.as_deref().unwrap_or("unusable")
                );
            }
            _ => {}
        }
        Ok(Some(row.materialized))
    }

    /// The last `max_bytes` of the execution's worker-log artifact, if the spoke
    /// sent one.
    pub fn worker_log_tail(&self, execution_id: &str, max_bytes: u64) -> Result<Option<String>> {
        let rows = self.db.spoke_custody_for_execution(execution_id)?;
        let Some(row) = rows.iter().find(|row| {
            row.kind == "artifact"
                && row.artifact_kind == Some(ArtifactKind::WorkerLog as i32)
                && row.materialized > 0
                && !matches!(row.state, CustodyState::Halted | CustodyState::Invalid)
        }) else {
            return Ok(None);
        };
        let path = self.root.join(&row.path);
        Ok(Some(
            write::tail(&path, row.materialized, max_bytes).with_context(|| format!("reading {}", path.display()))?,
        ))
    }

    // ── Reading structured output ───────────────────────────────────────────

    /// What the hub holds of the execution's structured output of `kind`.
    pub fn output_evidence(&self, execution_id: &str, kind: StructuredOutputKind) -> Result<OutputEvidence> {
        let rows = self.db.spoke_custody_for_execution(execution_id)?;
        let Some(latest) = rows.first().map(|row| row.assignment_generation) else {
            return Ok(OutputEvidence::Absent);
        };
        let rows: Vec<&SpokeCustody> = rows.iter().filter(|row| row.assignment_generation == latest).collect();
        let pr_url = kind == StructuredOutputKind::PrUrl;
        let artifact = rows
            .iter()
            .filter(|row| row.kind == "artifact" && output_matches(row, kind))
            .max_by_key(|row| row.source_sequence);
        let gap_name = if pr_url { PR_URL_OUTPUT } else { STRUCTURED_OUTPUT };
        let gap = rows.iter().find(|row| row.resource == gap_name);
        let capture = rows.iter().find(|row| row.resource == OUTPUT_CAPTURE);
        let from_gap = |gap: &SpokeCustody| {
            let reason = gap.reason.clone().unwrap_or_default();
            match gap.state {
                CustodyState::Invalid => OutputEvidence::Invalid(reason),
                _ => OutputEvidence::Unavailable(reason),
            }
        };
        Ok(match (artifact, gap) {
            // The newer of the two wins: a corrected file after a bad one, or a
            // bad rewrite after a good one.
            (Some(artifact), Some(gap)) if gap.source_sequence > artifact.source_sequence => from_gap(gap),
            (Some(artifact), _) => self.artifact_evidence(artifact, kind),
            (None, Some(gap)) => from_gap(gap),
            (None, None) => {
                if expected_output(&rows, kind) {
                    OutputEvidence::Incomplete { have: 0, total: 0 }
                } else {
                    capture.map_or(OutputEvidence::Absent, |gap| from_gap(gap))
                }
            }
        })
    }

    fn artifact_evidence(&self, row: &SpokeCustody, kind: StructuredOutputKind) -> OutputEvidence {
        let reason = row.reason.clone().unwrap_or_default();
        match row.state {
            CustodyState::Open => OutputEvidence::Incomplete {
                have: row.materialized,
                total: row.total_bytes.unwrap_or(0),
            },
            CustodyState::Invalid => OutputEvidence::Invalid(reason),
            CustodyState::Halted | CustodyState::Unavailable => OutputEvidence::Unavailable(reason),
            CustodyState::Complete => {
                let path = self.root.join(&row.path);
                match std::fs::read(&path) {
                    Ok(bytes) => match validate_output(kind, &bytes) {
                        Ok(()) => OutputEvidence::Ready {
                            path,
                            bytes: bytes.len() as u64,
                        },
                        Err(reason) => OutputEvidence::Invalid(reason),
                    },
                    Err(error) => OutputEvidence::Unavailable(format!("the hub's copy cannot be read: {error}")),
                }
            }
        }
    }

    /// Install the execution's verified output of `kind` at `destination`, the
    /// place the engine's structured-output readers look. Atomic: a reader sees
    /// the whole file or none.
    pub fn collect_output(
        &self,
        execution_id: &str,
        kind: StructuredOutputKind,
        destination: &Path,
    ) -> Result<Collected> {
        let path = match self.output_evidence(execution_id, kind)? {
            OutputEvidence::Ready { path, .. } => path,
            OutputEvidence::Incomplete { have, total } => {
                return Ok(Collected::NotAvailable(format!(
                    "the output is still arriving ({have} of {total} bytes)"
                )));
            }
            OutputEvidence::Unavailable(reason) => return Ok(Collected::NotAvailable(reason)),
            OutputEvidence::Absent => return Ok(Collected::NotAvailable("no output was registered".to_owned())),
            OutputEvidence::Invalid(reason) => return Ok(Collected::Failed(reason)),
        };
        let install = || -> std::io::Result<()> {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let staged = destination.with_extension(format!("{}.pulling", kind.slug()));
            std::fs::copy(&path, &staged)?;
            std::fs::File::open(&staged)?.sync_all()?;
            std::fs::rename(&staged, destination)
        };
        Ok(match install() {
            Ok(()) => Collected::Installed,
            Err(error) => Collected::Failed(format!(
                "installing the collected output for execution {execution_id} at {}: {error}",
                destination.display()
            )),
        })
    }

    // ── Retention ───────────────────────────────────────────────────────────

    /// Remove an execution's recorded custody files and ledger rows. The caller
    /// decides that the execution is finished and past retention; this never
    /// judges liveness. Only paths the ledger recorded are deleted. Returns the
    /// bytes those files occupied.
    pub fn reclaim_execution(&self, execution_id: &str) -> Result<u64> {
        let rows = self.db.spoke_custody_for_execution(execution_id)?;
        let mut bytes = 0;
        let mut parents = Vec::new();
        for row in &rows {
            if row.path.is_empty() {
                continue;
            }
            let path = self.root.join(&row.path);
            if let Ok(meta) = std::fs::metadata(&path) {
                bytes += meta.len();
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).with_context(|| format!("removing {}", path.display())),
            }
            if let Some(parent) = path.parent() {
                parents.push(parent.to_path_buf());
            }
        }
        parents.sort();
        parents.dedup();
        for parent in parents.into_iter().rev() {
            let _ = std::fs::remove_dir(&parent);
        }
        if let Ok(spoke) = segment(execution_id).map(|id| self.root.join(id).join("spoke")) {
            let _ = std::fs::remove_dir(&spoke);
            if let Some(execution) = spoke.parent() {
                let _ = std::fs::remove_dir(execution);
            }
        }
        // Rows go after the files: a crash between leaves rows whose files are
        // already gone, which the next pass removes.
        self.db.forget_spoke_custody(execution_id)?;
        Ok(bytes)
    }
}

struct Spec {
    kind: &'static str,
    resource: String,
    relative: String,
    limit: u64,
    artifact: Option<ArtifactSpec>,
}

struct ArtifactSpec {
    kind: i32,
    total: u64,
    digest: String,
    metadata: String,
}

fn halted(resource: &str, row: &SpokeCustody) -> Applied {
    Applied::Halted {
        resource: resource.to_owned(),
        reason: row.reason.clone().unwrap_or_else(|| "halted".to_owned()),
    }
}

fn bounded(text: &str) -> String {
    text.chars().take(MAX_REASON_CHARS).collect()
}

/// A peer-supplied identity as one path segment, or an error. Identities reach
/// here only after the ledger confirmed the host owns the run, but a path is
/// built from them, so they are checked anyway.
fn segment(text: &str) -> Result<&str> {
    let safe = !text.is_empty()
        && text.len() <= 128
        && text != "."
        && text != ".."
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if safe {
        Ok(text)
    } else {
        Err(anyhow!("`{}` is not a safe path segment", text.escape_debug()))
    }
}

/// The whole-content SHA-256 an artifact's identity carries: `<kind>:<64 hex>`,
/// with `kind` matching the chunk's [`ArtifactKind`].
fn identity_digest(artifact_id: &str, kind: i32) -> std::result::Result<String, String> {
    let Some((prefix, digest)) = artifact_id.rsplit_once(':') else {
        return Err("the artifact identity is not kind:digest".into());
    };
    if prefix != kind.to_string() {
        return Err(format!(
            "the artifact identity names kind {prefix}, not the chunk's kind {kind}"
        ));
    }
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("the artifact identity does not carry a SHA-256 digest".into());
    }
    Ok(digest.to_ascii_lowercase())
}

fn verified(path: &Path, artifact: &ArtifactSpec) -> std::result::Result<(), String> {
    let actual = write::digest_hex(path, artifact.total)?;
    if artifact.digest != actual {
        return Err(format!(
            "the artifact's bytes hash to {actual}, not the {} its identity names",
            artifact.digest
        ));
    }
    Ok(())
}

fn expected_output(rows: &[&SpokeCustody], kind: StructuredOutputKind) -> bool {
    let Some(concluded) = rows
        .iter()
        .find(|row| row.resource == OUTPUT_CONCLUDED || row.resource.starts_with("output:concluded:"))
    else {
        return false;
    };
    let ids: Vec<String> = serde_json::from_str(concluded.reason.as_deref().unwrap_or("[]")).unwrap_or_default();
    ids.iter().any(|id| {
        let prefix = id.split_once(':').map(|(kind, _)| kind);
        match kind {
            StructuredOutputKind::PrUrl => prefix == Some(&(ArtifactKind::PrUrl as i32).to_string()),
            _ => prefix == Some(&(ArtifactKind::ReviewResult as i32).to_string()),
        }
    })
}

fn output_matches(row: &SpokeCustody, kind: StructuredOutputKind) -> bool {
    match (kind, row.artifact_kind) {
        (StructuredOutputKind::PrUrl, Some(raw)) => raw == ArtifactKind::PrUrl as i32,
        (_, Some(raw)) if raw == ArtifactKind::ReviewResult as i32 => {
            let declared: Option<String> = row
                .metadata_json
                .as_deref()
                .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                .and_then(|value| value.get("output_kind")?.as_str().map(str::to_owned));
            declared.as_deref() == Some(kind.slug())
        }
        _ => false,
    }
}

/// The checks every output must pass before an engine reader sees it. The
/// readers validate the payload's schema again; these catch what makes a file
/// not worth installing at all.
fn validate_output(kind: StructuredOutputKind, bytes: &[u8]) -> std::result::Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| format!("the output is not JSON: {error}"))?;
    if kind == StructuredOutputKind::PrUrl {
        let payload: PrUrlPayload =
            serde_json::from_value(value).map_err(|error| format!("the PR-url output is malformed: {error}"))?;
        if payload.pr_url.trim().is_empty() {
            return Err("the PR-url output names no URL".to_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! What the hub holds of a spoke's bytes, over a real on-disk `WorkDb` and
    //! real files. A "crash" is the write reaching the disk without the ledger
    //! row, or the same record applied again after a restart.

    use super::*;
    use boss_engine_structured_output::path_for;

    struct Fixture {
        dir: tempfile::TempDir,
        custody: Custody,
        db: WorkDb,
        run: Run,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("work.db")).unwrap();
        let custody = Custody::new(db.clone(), dir.path().join("custody"));
        let run = Run {
            execution_id: "exec_1".into(),
            run_id: "run_1".into(),
            assignment_generation: 1,
            spawn_token: "token".into(),
        };
        Fixture { dir, custody, db, run }
    }

    impl Fixture {
        fn source(&self, sequence: u64) -> Source<'_> {
            Source {
                host_id: "host-a",
                run: &self.run,
                sequence,
            }
        }

        fn transcript(&self, sequence: u64, generation: &str, offset: u64, data: &[u8]) -> Applied {
            self.custody
                .apply_transcript(&self.source(sequence), &delta(generation, offset, data))
                .unwrap()
        }

        fn artifact(&self, sequence: u64, chunk: ArtifactChunk) -> Applied {
            self.custody.apply_artifact(&self.source(sequence), &chunk).unwrap()
        }

        fn gap(&self, sequence: u64, resource: &str, from: u64, reason: &str) -> Applied {
            let gap = Gap {
                resource: resource.into(),
                from_offset: from,
                through_offset: from,
                reason: reason.into(),
            };
            self.custody.apply_gap(&self.source(sequence), &gap).unwrap()
        }

        fn row(&self, resource: &str) -> boss_work_db::SpokeCustody {
            self.db
                .spoke_custody("exec_1", 1, resource)
                .unwrap()
                .expect("a ledger row")
        }

        fn file(&self, resource: &str) -> PathBuf {
            self.dir.path().join("custody").join(self.row(resource).path)
        }
    }

    fn delta(generation: &str, offset: u64, data: &[u8]) -> TranscriptDelta {
        TranscriptDelta {
            generation: generation.into(),
            offset,
            data: data.to_vec(),
            sha256: Sha256::digest(data).to_vec(),
            eof: false,
        }
    }

    fn chunk(
        id: &str,
        kind: ArtifactKind,
        whole: &[u8],
        offset: usize,
        len: usize,
        output_kind: &str,
    ) -> ArtifactChunk {
        let data = &whole[offset..offset + len];
        ArtifactChunk {
            artifact_id: id.into(),
            kind: kind as i32,
            offset: offset as u64,
            data: data.to_vec(),
            sha256: Sha256::digest(data).to_vec(),
            eof: offset + len == whole.len(),
            total_bytes: whole.len() as u64,
            metadata_json: format!(r#"{{"output_kind":"{output_kind}"}}"#).into_bytes(),
        }
    }

    fn identity(kind: ArtifactKind, whole: &[u8]) -> String {
        let digest: String = Sha256::digest(whole).iter().map(|b| format!("{b:02x}")).collect();
        format!("{}:{digest}", kind as i32)
    }

    fn is_halted(applied: &Applied) -> bool {
        matches!(applied, Applied::Halted { .. })
    }

    #[test]
    fn chunks_are_written_at_their_offsets_and_the_ledger_follows_the_disk() {
        let f = fixture();
        assert!(matches!(
            f.transcript(1, "0-1", 0, b"{\"a\":1}\n"),
            Applied::Wrote {
                first: true,
                materialized: 8,
                ..
            }
        ));
        assert!(matches!(
            f.transcript(2, "0-1", 8, b"{\"b\":2}\n"),
            Applied::Wrote {
                first: false,
                materialized: 16,
                ..
            }
        ));
        assert_eq!(
            std::fs::read(f.file("transcript:0-1")).unwrap(),
            b"{\"a\":1}\n{\"b\":2}\n"
        );
        assert_eq!(f.row("transcript:0-1").materialized, 16);
    }

    #[test]
    fn replaying_a_record_after_a_restart_writes_nothing_twice() {
        let f = fixture();
        f.transcript(1, "0-1", 0, b"hello ");
        f.transcript(2, "0-1", 6, b"world");
        // The ledger and the cursor were both committed, then the projector
        // restarted and met the same records again.
        assert!(matches!(f.transcript(1, "0-1", 0, b"hello "), Applied::Replayed { .. }));
        assert!(matches!(f.transcript(2, "0-1", 6, b"world"), Applied::Replayed { .. }));
        assert_eq!(std::fs::read(f.file("transcript:0-1")).unwrap(), b"hello world");
    }

    #[test]
    fn a_crash_between_the_file_and_the_ledger_is_repaired_by_the_replay() {
        let f = fixture();
        f.transcript(1, "0-1", 0, b"abc");
        // The next chunk reached the disk (with extra bytes a longer earlier
        // attempt left) but the ledger row never moved.
        std::fs::write(f.file("transcript:0-1"), b"abcdefSTALE").unwrap();
        assert_eq!(f.row("transcript:0-1").materialized, 3);
        assert!(matches!(
            f.transcript(2, "0-1", 3, b"def"),
            Applied::Wrote { materialized: 6, .. }
        ));
        assert_eq!(std::fs::read(f.file("transcript:0-1")).unwrap(), b"abcdef");
    }

    #[test]
    fn a_hole_a_changed_overlap_and_a_bad_digest_each_halt_only_that_resource() {
        let f = fixture();
        f.transcript(1, "0-1", 0, b"abcd");
        assert!(is_halted(&f.transcript(2, "0-1", 9, b"x")), "a hole is not filled in");
        // The halt is reported again by later records, so whichever one reaches the
        // projector first raises it.
        let again = f.transcript(3, "0-1", 4, b"e");
        assert!(matches!(&again, Applied::Halted { reason, .. } if reason.contains("expected bytes from offset 4")));
        assert_eq!(f.row("transcript:0-1").state, CustodyState::Halted);
        assert_eq!(f.row("transcript:0-1").materialized, 4, "the held prefix is kept");

        f.transcript(4, "1-1", 0, b"abcd");
        assert!(
            is_halted(&f.transcript(5, "1-1", 2, b"ZZ")),
            "a changed overlap is not accepted"
        );

        let mut bad = delta("2-1", 0, b"abcd");
        bad.sha256 = vec![7; 32];
        assert!(is_halted(&f.custody.apply_transcript(&f.source(6), &bad).unwrap()));
        // A generation nobody damaged is untouched.
        assert!(matches!(f.transcript(7, "3-1", 0, b"ok"), Applied::Wrote { .. }));
    }

    #[test]
    fn watermarks_say_pending_until_the_bytes_are_held_and_ready_after() {
        let f = fixture();
        let mark = Mark::new("0-1", 10);
        assert_eq!(
            f.custody.transcript_evidence(&f.run, &mark, false).unwrap(),
            Evidence::Pending {
                have: 0,
                want: 10,
                cause: PendingCause::Transcript
            }
        );
        f.transcript(1, "0-1", 0, b"01234");
        assert_eq!(
            f.custody.transcript_evidence(&f.run, &mark, false).unwrap(),
            Evidence::Pending {
                have: 5,
                want: 10,
                cause: PendingCause::Transcript
            }
        );
        f.transcript(2, "0-1", 5, b"56789");
        let Evidence::Ready(source) = f.custody.transcript_evidence(&f.run, &mark, false).unwrap() else {
            panic!("the mark is met");
        };
        assert_eq!(std::fs::read(&source.path).unwrap(), b"0123456789");
        assert!(
            source.path.starts_with(&source.root),
            "readers can confine the path to the run's directory"
        );
        assert_eq!(f.custody.read_tail_at(&source.path, 4).unwrap().unwrap(), "6789");
    }

    #[test]
    fn gaps_are_unavailable_evidence_never_a_complete_transcript() {
        let f = fixture();
        f.transcript(1, "0-1", 0, b"abcde");
        // The source was replaced; whatever the spoke did not read is gone.
        assert_eq!(f.gap(2, "transcript:0-1", 5, "source replaced"), Applied::Recorded);
        let reason = |e: Evidence| match e {
            Evidence::Unavailable(reason) => reason,
            other => panic!("expected unavailable, got {other:?}"),
        };
        assert!(
            reason(
                f.custody
                    .transcript_evidence(&f.run, &Mark::new("0-1", 9), false)
                    .unwrap()
            )
            .contains("source replaced")
        );
        // Bytes the hub does hold still satisfy a mark inside them.
        assert!(matches!(
            f.custody
                .transcript_evidence(&f.run, &Mark::new("0-1", 5), false)
                .unwrap(),
            Evidence::Ready(_)
        ));
        // A closed resource takes no new bytes, but a replay of held ones is fine.
        assert_eq!(
            f.transcript(3, "0-1", 5, b"more"),
            Applied::Ignored("the resource is closed")
        );
        assert!(matches!(f.transcript(1, "0-1", 0, b"abcde"), Applied::Replayed { .. }));

        // No generation and a bare gap: the spoke said there is no transcript.
        f.gap(4, "transcript", 0, "transcript unavailable at Stop: not attached");
        assert!(
            reason(f.custody.transcript_evidence(&f.run, &Mark::default(), false).unwrap()).contains("not attached")
        );
        // A gap that starts after bytes the hub never received is not trusted.
        assert!(is_halted(&f.gap(5, "transcript:9-9", 40, "lost")));
        // An informational gap changes nothing.
        assert!(matches!(
            f.gap(6, "transcript:0-1:entry", 0, "oversized line"),
            Applied::Ignored(_)
        ));
    }

    #[test]
    fn an_artifact_arriving_in_pieces_is_complete_only_when_its_digest_matches() {
        let f = fixture();
        let whole = br#"{"pr_url":"https://github.com/o/r/pull/1"}"#;
        let id = identity(ArtifactKind::PrUrl, whole);
        f.artifact(1, chunk(&id, ArtifactKind::PrUrl, whole, 0, 10, ""));
        assert_eq!(f.row(&format!("artifact:{id}")).state, CustodyState::Open);
        assert_eq!(
            f.custody
                .output_evidence("exec_1", StructuredOutputKind::PrUrl)
                .unwrap(),
            OutputEvidence::Incomplete {
                have: 10,
                total: whole.len() as u64
            }
        );
        f.gap(2, OUTPUT_CONCLUDED, 0, &format!(r#"["{id}"]"#));
        // A settled boundary does not read it half-written.
        assert!(matches!(
            f.custody
                .transcript_evidence(&f.run, &Mark::new("0-1", 0), true)
                .unwrap(),
            Evidence::Pending { have: 10, .. }
        ));
        f.artifact(3, chunk(&id, ArtifactKind::PrUrl, whole, 10, whole.len() - 10, ""));
        assert_eq!(f.row(&format!("artifact:{id}")).state, CustodyState::Complete);

        let destination = path_for(&f.dir.path().join("out"), "exec_1", StructuredOutputKind::PrUrl);
        assert_eq!(
            f.custody
                .collect_output("exec_1", StructuredOutputKind::PrUrl, &destination)
                .unwrap(),
            Collected::Installed
        );
        assert_eq!(std::fs::read(destination).unwrap(), whole);
    }

    #[test]
    fn structured_output_that_is_unusable_is_distinct_from_output_that_is_missing() {
        let kind = StructuredOutputKind::ReviewResult;
        let out = |f: &Fixture| f.dir.path().join("out").join("review.json");
        let collect = |f: &Fixture| f.custody.collect_output("exec_1", kind, &out(f)).unwrap();

        // Nothing registered: not available, not failed.
        let f = fixture();
        assert!(matches!(collect(&f), Collected::NotAvailable(_)));

        // Bytes that do not hash to their identity are positively bad.
        let whole = br#"{"findings":[]}"#;
        let wrong_id = identity(ArtifactKind::ReviewResult, b"something else");
        f.artifact(
            1,
            chunk(
                &wrong_id,
                ArtifactKind::ReviewResult,
                whole,
                0,
                whole.len(),
                "review-result",
            ),
        );
        assert!(matches!(collect(&f), Collected::Failed(reason) if reason.contains("hash to")));
        assert!(!out(&f).exists(), "nothing is installed from a bad artifact");

        // Complete but not JSON.
        let f = fixture();
        let text = b"not json at all";
        let id = identity(ArtifactKind::ReviewResult, text);
        f.artifact(
            1,
            chunk(&id, ArtifactKind::ReviewResult, text, 0, text.len(), "review-result"),
        );
        assert!(matches!(collect(&f), Collected::Failed(reason) if reason.contains("not JSON")));

        // A PR-url artifact with no URL.
        let f = fixture();
        let empty = br#"{"pr_url":" "}"#;
        let id = identity(ArtifactKind::PrUrl, empty);
        f.artifact(1, chunk(&id, ArtifactKind::PrUrl, empty, 0, empty.len(), ""));
        let pr = f
            .custody
            .output_evidence("exec_1", StructuredOutputKind::PrUrl)
            .unwrap();
        assert!(matches!(pr, OutputEvidence::Invalid(reason) if reason.contains("no URL")));

        // The spoke refused the file: bad. The spool had no room: unknown.
        let f = fixture();
        f.gap(
            1,
            "output:BOSS_STRUCTURED_OUTPUT",
            0,
            "output is not JSON: expected value",
        );
        assert!(matches!(collect(&f), Collected::Failed(reason) if reason.contains("not JSON")));
        let f = fixture();
        f.gap(1, "output:capture", 0, "the spool had no room after 5 attempts");
        assert!(matches!(collect(&f), Collected::NotAvailable(reason) if reason.contains("no room")));

        // A corrected file after a bad one wins; a bad rewrite after a good one does too.
        let f = fixture();
        f.gap(1, "output:BOSS_STRUCTURED_OUTPUT", 0, "output is not JSON");
        let good = br#"{"findings":[]}"#;
        let id = identity(ArtifactKind::ReviewResult, good);
        f.artifact(
            2,
            chunk(&id, ArtifactKind::ReviewResult, good, 0, good.len(), "review-result"),
        );
        assert_eq!(collect(&f), Collected::Installed);
        f.gap(3, "output:BOSS_STRUCTURED_OUTPUT", 0, "output exceeds limit");
        assert!(matches!(collect(&f), Collected::Failed(_)));

        // Another kind's artifact is not this kind's output.
        let f = fixture();
        f.artifact(
            1,
            chunk(&id, ArtifactKind::ReviewResult, good, 0, good.len(), "followups"),
        );
        assert!(matches!(collect(&f), Collected::NotAvailable(_)));
    }

    #[test]
    fn artifact_chunks_that_contradict_themselves_halt_the_artifact() {
        let f = fixture();
        let whole = br#"{"a":1}"#;
        let id = identity(ArtifactKind::ReviewResult, whole);
        f.artifact(1, chunk(&id, ArtifactKind::ReviewResult, whole, 0, 3, "review-result"));
        let mut lying = chunk(
            &id,
            ArtifactKind::ReviewResult,
            whole,
            3,
            whole.len() - 3,
            "review-result",
        );
        lying.total_bytes += 5;
        assert!(is_halted(&f.artifact(2, lying)));

        let g = fixture();
        let mut early = chunk(&id, ArtifactKind::ReviewResult, whole, 0, 3, "review-result");
        early.eof = true;
        assert!(is_halted(&g.artifact(1, early)), "eof before the declared total");

        let h = fixture();
        let big = Custody::new(h.db.clone(), h.dir.path().join("small")).with_limits(Limits {
            max_transcript_bytes: 4,
            max_artifact_bytes: 4,
        });
        let applied = big
            .apply_transcript(&h.source(1), &delta("0-1", 0, b"too long"))
            .unwrap();
        assert!(matches!(applied, Applied::Halted { reason, .. } if reason.contains("custody limit")));
    }

    #[test]
    fn reclaim_removes_the_recorded_files_and_rows_and_nothing_else() {
        let f = fixture();
        f.transcript(1, "0-1", 0, b"abc");
        let kept = f.dir.path().join("custody").join("other_exec").join("spoke");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::write(kept.join("keep"), b"x").unwrap();
        let file = f.file("transcript:0-1");
        assert_eq!(f.custody.reclaim_execution("exec_1").unwrap(), 3);
        assert!(!file.exists());
        assert!(f.db.spoke_custody_for_execution("exec_1").unwrap().is_empty());
        assert!(kept.join("keep").exists());
        // Idempotent once gone.
        assert_eq!(f.custody.reclaim_execution("exec_1").unwrap(), 0);
    }

    #[test]
    fn a_run_identity_cannot_name_a_path_outside_the_custody_root() {
        let f = fixture();
        let evil = Run {
            execution_id: "../escape".into(),
            ..f.run.clone()
        };
        let source = Source {
            host_id: "host-a",
            run: &evil,
            sequence: 1,
        };
        assert!(f.custody.apply_transcript(&source, &delta("0-1", 0, b"x")).is_err());
        assert!(!f.dir.path().join("escape").exists());
    }

    #[test]
    fn a_malformed_artifact_identity_is_invalid_not_complete() {
        let f = fixture();
        let whole = br#"{"pr_url":"https://github.com/o/r/pull/1"}"#;
        let mut bad = chunk("not-a-digest", ArtifactKind::PrUrl, whole, 0, whole.len(), "");
        bad.artifact_id = "4:zzzz".into();
        assert!(matches!(
            f.artifact(1, bad),
            Applied::Recorded | Applied::Halted { .. } | Applied::Ignored(_)
        ));
        let row = f.row("artifact:4:zzzz");
        assert_eq!(row.state, CustodyState::Invalid);
        assert!(row.reason.as_deref().unwrap_or("").contains("SHA-256 digest"));

        let g = fixture();
        let mut wrong_kind = chunk(
            &identity(ArtifactKind::PrUrl, whole),
            ArtifactKind::ReviewResult,
            whole,
            0,
            whole.len(),
            "review-result",
        );
        wrong_kind.artifact_id = identity(ArtifactKind::PrUrl, whole);
        assert!(matches!(g.artifact(1, wrong_kind), Applied::Recorded));
        assert_eq!(
            g.row(&format!("artifact:{}", identity(ArtifactKind::PrUrl, whole)))
                .state,
            CustodyState::Invalid
        );
    }

    #[test]
    fn writing_past_the_declared_total_halts_and_an_artifact_gap_closes_it() {
        let f = fixture();
        let whole = br#"{"a":1}"#;
        let id = identity(ArtifactKind::ReviewResult, whole);
        let mut over = chunk(&id, ArtifactKind::ReviewResult, whole, 0, whole.len(), "review-result");
        over.total_bytes = 2;
        over.eof = false;
        assert!(is_halted(&f.artifact(1, over)));

        let g = fixture();
        g.artifact(1, chunk(&id, ArtifactKind::ReviewResult, whole, 0, 3, "review-result"));
        assert_eq!(g.row(&format!("artifact:{id}")).state, CustodyState::Open);
        g.gap(2, &format!("artifact:{id}"), 3, "the source vanished");
        assert_eq!(g.row(&format!("artifact:{id}")).state, CustodyState::Unavailable);
    }

    #[test]
    fn a_named_output_with_no_row_stays_pending_until_capture_concludes() {
        let f = fixture();
        f.transcript(1, "0-1", 0, b"hello");
        assert!(matches!(
            f.custody
                .transcript_evidence(&f.run, &Mark::new("0-1", 5), true)
                .unwrap(),
            Evidence::Pending { .. }
        ));
        let whole = br#"{"pr_url":"https://github.com/o/r/pull/1"}"#;
        let id = identity(ArtifactKind::PrUrl, whole);
        f.gap(2, OUTPUT_CONCLUDED, 0, &format!(r#"["{id}"]"#));
        assert!(matches!(
            f.custody
                .transcript_evidence(&f.run, &Mark::new("0-1", 5), true)
                .unwrap(),
            Evidence::Pending { have: 0, want: 1, .. }
        ));
        assert_eq!(
            f.custody
                .output_evidence("exec_1", StructuredOutputKind::PrUrl)
                .unwrap(),
            OutputEvidence::Incomplete { have: 0, total: 0 }
        );
        f.artifact(3, chunk(&id, ArtifactKind::PrUrl, whole, 0, whole.len(), ""));
        let Evidence::Ready(_) = f
            .custody
            .transcript_evidence(&f.run, &Mark::new("0-1", 5), true)
            .unwrap()
        else {
            panic!("named output is collectable once complete");
        };
    }

    #[test]
    fn a_halted_transcript_is_not_served_and_reads_stop_at_the_watermark() {
        let f = fixture();
        f.transcript(1, "0-1", 0, b"abcdef");
        let path = f.file("transcript:0-1");
        std::fs::write(&path, b"abcdefSTALE").unwrap();
        assert_eq!(f.custody.read_tail_at(&path, 20).unwrap().as_deref(), Some("abcdef"));
        f.gap(2, "transcript:0-1", 0, "source replaced");
        // Unavailable still serves the committed prefix.
        assert_eq!(f.custody.read_tail_at(&path, 20).unwrap().as_deref(), Some("abcdef"));
        let g = fixture();
        g.transcript(1, "0-1", 0, b"abcd");
        assert!(is_halted(&g.transcript(2, "0-1", 20, b"zz")));
        let halted = g.file("transcript:0-1");
        assert!(
            g.custody
                .read_tail_at(&halted, 20)
                .unwrap_err()
                .to_string()
                .contains("halted"),
            "a halted copy is not served as ordinary text"
        );
    }
}
