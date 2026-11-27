//! Per-generation directories for the coordinator chat backend, and the
//! policy that prunes old ones.
//!
//! Each coordinator generation gets one directory under the engine's chat
//! root, created with mode `0700`. It holds everything the engine needs to
//! rebuild that generation's conversation:
//!
//! | file            | writer               | content                                   |
//! | --------------- | -------------------- | ----------------------------------------- |
//! | `wire.jsonl`    | the agent host       | every line, both directions (the journal) |
//! | `intent.jsonl`  | the engine           | prompts queued, sent and withdrawn        |
//! | `snapshot.json` | the engine           | cached reducer state (a cache only)       |
//! | `images/`       | the engine           | images sent with prompts                  |
//!
//! The live generation is never pruned. Every other generation is pruned by
//! age first, then by total size, oldest first.
//!
//! # Measurement behind the defaults
//!
//! `retention_tests::measure_generation_disk_cost` builds a 250-turn
//! generation through the real writers, sized from the coordinator ACP spike's
//! per-turn traffic (about 25 wire lines a turn, streamed answers in deltas of
//! about 15 characters, tool calls with fenced output, and the agent's
//! 27,587-byte `available_commands_update` re-sent every 50 turns), and
//! prints what it costs on disk:
//!
//! | file            | 250 turns      | per turn  |
//! | --------------- | -------------- | --------- |
//! | `wire.jsonl`    | 2,595,700 B    | 10.4 KB   |
//! | `intent.jsonl`  | 72,047 B       | 0.3 KB    |
//! | `snapshot.json` | 790,981 B      | 3.2 KB    |
//! | total           | 3,458,728 B    | 13.8 KB   |
//!
//! The cost is linear in turns, and the wire journal is most of it. The
//! numbers are from a synthetic generation: no long-lived real generation
//! has been measured, and the spike sampled 25 short conversations. What
//! is not measured is how many turns a day a real coordinator takes; the
//! defaults assume at most 1,000 (`ASSUMED_MAX_TURNS_PER_DAY`), which the
//! test pins:
//!
//! - [`DEFAULT_MAX_AGE`] is 14 days: two weeks of conversation history.
//!   This one is chosen, not measured: nothing measures how far back
//!   anyone looks. It is cheap to keep because the size cap, not this, is
//!   what protects the disk.
//! - [`DEFAULT_MAX_TOTAL_BYTES`] is 256 MiB: 14 days at the assumed ceiling
//!   is about 194 MB at the measured per-turn cost, so the cap does not cut
//!   in before the age rule in normal use. It is the bound for a runaway
//!   generation, not the usual reason a directory goes.
//!
//! If a real coordinator turns out far above the assumed turn rate, the cap
//! is what protects the disk and the test's arithmetic is what to revisit.

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boss_engine_driver_home_retention::{OwnedHome, plan_reclaim};

/// The agent host's journal, written by `boss-agent-host --journal`.
pub const WIRE_FILE_NAME: &str = "wire.jsonl";
/// The engine's intent log ([`crate::intent::IntentLog`]).
pub const INTENT_FILE_NAME: &str = "intent.jsonl";
/// The reducer snapshot cache ([`crate::snapshot`]).
pub const SNAPSHOT_FILE_NAME: &str = "snapshot.json";
/// Images sent with prompts.
pub const IMAGES_DIR_NAME: &str = "images";

/// A non-live generation whose newest file is older than this is removed.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// After the age rule, the oldest non-live generations are removed until the
/// non-live generations together fit in this many bytes.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// The turn rate the defaults are sized for. A ceiling, not an observation.
pub const ASSUMED_MAX_TURNS_PER_DAY: u64 = 1_000;

/// One coordinator generation's directory and the files in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    id: String,
    dir: PathBuf,
}

impl Generation {
    /// The generation id, which is also the directory's name. It sorts by
    /// creation time.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn wire_path(&self) -> PathBuf {
        self.dir.join(WIRE_FILE_NAME)
    }

    pub fn intent_path(&self) -> PathBuf {
        self.dir.join(INTENT_FILE_NAME)
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.dir.join(SNAPSHOT_FILE_NAME)
    }

    pub fn images_dir(&self) -> PathBuf {
        self.dir.join(IMAGES_DIR_NAME)
    }
}

/// How old generations are pruned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// A non-live generation whose newest file is older than this is removed.
    pub max_age: Duration,
    /// After the age rule, the oldest non-live generations are removed until
    /// the rest total at most this many bytes.
    pub max_total_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age: DEFAULT_MAX_AGE,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

/// Why a generation was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneReason {
    /// Its newest file was older than [`RetentionPolicy::max_age`].
    Age,
    /// The non-live generations exceeded [`RetentionPolicy::max_total_bytes`]
    /// and this was the oldest left.
    Size,
}

/// A generation [`GenerationStore::prune`] removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedGeneration {
    pub id: String,
    pub bytes: u64,
    pub reason: PruneReason,
}

/// What one [`GenerationStore::prune`] pass did.
#[derive(Debug, Default)]
pub struct PruneReport {
    pub removed: Vec<PrunedGeneration>,
    /// Generations that could not be removed, with the reason. They stay on
    /// disk and are tried again on the next pass.
    pub failed: Vec<(String, io::Error)>,
    /// Non-live generations left after the pass.
    pub kept: usize,
    /// Bytes those generations hold.
    pub kept_bytes: u64,
}

/// The directory that holds every generation's directory.
#[derive(Debug, Clone)]
pub struct GenerationStore {
    root: PathBuf,
}

impl GenerationStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates the directory for a new generation, mode `0700`, and the chat
    /// root above it if it does not exist yet.
    pub fn create(&self) -> io::Result<Generation> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.root)?;
        loop {
            let id = new_generation_id();
            let dir = self.root.join(&id);
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(Generation { id, dir }),
                // The counter in the id moves on, so the next try differs.
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
    }

    /// Starts a new generation and prunes the old ones in the same step, with
    /// the new generation as the live one. What the coordinator lifecycle
    /// calls when it launches a coordinator. Pruning that fails does not fail
    /// the launch: the report says what was and was not removed, and the next
    /// launch tries again.
    pub fn create_and_prune(&self, policy: &RetentionPolicy) -> io::Result<(Generation, PruneReport)> {
        let generation = self.create()?;
        let report = match self.prune(generation.id(), policy, SystemTime::now()) {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!(%error, root = %self.root.display(), "could not prune old coordinator chat generations");
                PruneReport::default()
            }
        };
        for (id, error) in &report.failed {
            tracing::warn!(%error, generation = id, "could not remove an old coordinator chat generation");
        }
        Ok((generation, report))
    }

    /// The existing generation `id`. Fails if `id` is not a generation id (so
    /// it can never name a path outside the root) or has no directory.
    pub fn open(&self, id: &str) -> io::Result<Generation> {
        if !is_generation_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{id:?} is not a coordinator generation id"),
            ));
        }
        let dir = self.root.join(id);
        if !fs::symlink_metadata(&dir)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("generation {id} has no directory"),
            ));
        }
        Ok(Generation { id: id.to_owned(), dir })
    }

    /// Removes the generations `policy` says to, never the one named
    /// `live_id`, and never anything in the root that is not a generation
    /// directory.
    pub fn prune(&self, live_id: &str, policy: &RetentionPolicy, now: SystemTime) -> io::Result<PruneReport> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(PruneReport::default()),
            Err(err) => return Err(err),
        };
        let mut report = PruneReport::default();
        let mut candidates = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if id == live_id || !is_generation_id(&id) || !entry.file_type()?.is_dir() {
                continue;
            }
            let footprint = match footprint(&entry.path()) {
                Ok(footprint) => footprint,
                Err(err) => {
                    report.failed.push((id, err));
                    continue;
                }
            };
            candidates.push((id, footprint));
        }

        let homes: Vec<OwnedHome> = candidates
            .iter()
            .map(|(id, footprint)| OwnedHome {
                path: self.root.join(id),
                execution_id: id.clone(),
                is_live: false,
                age_anchor_epoch: system_time_epoch_secs(footprint.last_activity),
                size_bytes: footprint.bytes,
            })
            .collect();
        let shared_policy =
            boss_engine_driver_home_retention::RetentionPolicy::new(policy.max_age, policy.max_total_bytes);
        let plan = plan_reclaim(&homes, system_time_epoch_secs(now), &shared_policy);
        let planned: std::collections::HashSet<PathBuf> = plan.to_delete.into_iter().collect();
        let now_epoch = system_time_epoch_secs(now);
        let max_age_secs = i64::try_from(policy.max_age.as_secs()).unwrap_or(i64::MAX);
        for (id, footprint) in &candidates {
            if !planned.contains(&self.root.join(id)) {
                continue;
            }
            let age_secs = now_epoch.saturating_sub(system_time_epoch_secs(footprint.last_activity));
            let reason = if age_secs > max_age_secs {
                PruneReason::Age
            } else {
                PruneReason::Size
            };
            self.remove(id, footprint.bytes, reason, &mut report);
        }
        let surviving: Vec<&Footprint> = candidates
            .iter()
            .filter(|(id, _)| !report.removed.iter().any(|removed| removed.id == *id))
            .map(|(_, footprint)| footprint)
            .collect();
        // A generation that could not be removed is still on disk.
        report.kept = surviving.len() + report.failed.len();
        report.kept_bytes = surviving.iter().map(|footprint| footprint.bytes).sum();
        Ok(report)
    }

    /// Removes one generation directory. Returns whether it is gone.
    fn remove(&self, id: &str, bytes: u64, reason: PruneReason, report: &mut PruneReport) -> bool {
        match fs::remove_dir_all(self.root.join(id)) {
            Ok(()) => {
                report.removed.push(PrunedGeneration {
                    id: id.to_owned(),
                    bytes,
                    reason,
                });
                true
            }
            Err(err) => {
                report.failed.push((id.to_owned(), err));
                false
            }
        }
    }
}

/// What a generation directory holds and when it was last written.
struct Footprint {
    bytes: u64,
    last_activity: SystemTime,
}

fn footprint(dir: &Path) -> io::Result<Footprint> {
    let mut footprint = Footprint {
        bytes: 0,
        last_activity: fs::symlink_metadata(dir)?.modified()?,
    };
    accumulate(dir, &mut footprint)?;
    Ok(footprint)
}

/// Adds the files under `dir` to `footprint`. Symbolic links count as
/// themselves and are never followed, so a link cannot pull another tree into
/// the count or the removal.
fn accumulate(dir: &Path, footprint: &mut Footprint) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        footprint.bytes += metadata.len();
        footprint.last_activity = footprint.last_activity.max(metadata.modified()?);
        if metadata.is_dir() {
            accumulate(&entry.path(), footprint)?;
        }
    }
    Ok(())
}

/// `<unix ms, 13 digits>-<pid hex>-<counter, 8 hex digits>`: unique on this
/// machine and sorting by creation time within one engine process.
fn new_generation_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{ms:013}-{:x}-{counter:08x}", std::process::id())
}

fn system_time_epoch_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn is_generation_id(id: &str) -> bool {
    let mut parts = id.split('-');
    let (Some(ms), Some(pid), Some(counter), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let hex = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    ms.len() >= 13 && ms.bytes().all(|byte| byte.is_ascii_digit()) && hex(pid) && hex(counter)
}
