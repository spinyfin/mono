//! Repo-declared workspace cleanup, driven by free disk space.
//!
//! cube keeps free workspaces warm on purpose — a leased workspace that lands
//! on a recently used checkout starts with its build state intact, and that
//! is most of what the pool is for. Some of that state lives *outside* the
//! checkout (a build tool's output tree under a shared cache directory), where
//! no per-workspace compaction can see it and nothing ever gives it back. On
//! 2026-10-21 that had grown to 641 GB across 86 workspaces and filled the
//! volume.
//!
//! This module is the relief valve, and it is deliberately ignorant of what
//! it is relieving. A repo declares a `cleanup:` command in `.cube/setup.yaml`
//! (see [`crate::setup::CleanupStep`]); cube decides **when** and **whether** a
//! workspace may be cleaned and the repo's command decides **what cleaning
//! means**. Nothing here names a build tool.
//!
//! ## When
//!
//! Free space on the workspace's volume is the only trigger. Below the
//! low-water mark (the lease path's existing floor,
//! [`crate::config::PoolConfig::free_space_floor_bytes`]) cube cleans free
//! workspaces least-recently-leased first, one at a time, re-reading free space
//! after each, and stops as soon as it is back above the high-water mark
//! ([`crate::config::PoolConfig::cleanup_high_water_bytes`]) — a deliberately
//! modest margin above the floor, so one episode does not cold-start a pile of
//! warm workspaces. Above the floor nothing is cleaned, whatever the idle time:
//! there is no retention clock.
//!
//! Least recently leased first is also least likely to be reused. Leases take
//! the lowest free workspace id, so a long tail of high-numbered workspaces sits
//! idle for weeks while a few low-numbered ones are re-leased within minutes;
//! ordering by last activity reaches the idle tail first and leaves the hot
//! ones warm.
//!
//! A workspace is not cleaned again until it has been leased again — cleaning an
//! already-clean tree frees nothing and would spend the pass on no-ops.
//!
//! ## Safety
//!
//! - **Only free workspaces.** A leased workspace is never a candidate.
//! - **Not mid-lease.** Cleaning starts with an atomic claim in the registry
//!   ([`Store::begin_workspace_cleanup`]) that every lease claim query honours,
//!   so a workspace being cleaned is never handed out: a lease arriving
//!   meanwhile takes another workspace or mints one. The claim is taken under
//!   the repo lock, like the pool trim's removal, so the trim cannot remove a
//!   workspace out from under it either.
//! - **One pass at a time.** A pass holds a cleanup lock (separate from the
//!   per-repo lease lock, so leases never wait on it) across its free-space
//!   decisions, the commands and the measurements. A manual pass waits its turn
//!   and then re-reads free space, so it cleans nothing the pass before it
//!   already made unnecessary; the periodic pass, which runs inside a lease,
//!   skips itself when another is running.
//! - **Not workspaces holding recoverable work.** Dirty, conflicted and
//!   quarantined workspaces are skipped: a recovery lease (`--allow-dirty`) must
//!   be able to claim them, and the aged-unhealthy pass owns their fate.
//! - **A failure is not a clean.** A cleanup that fails (or whose process dies)
//!   leaves the workspace marked and out of the pool — it is never leased in a
//!   half-cleaned state — and is retried, throttled, until it succeeds.
//! - **The repo's command may decline.** Exit status
//!   [`CLEANUP_BUSY_EXIT_CODE`] means "in use right now, nothing done": the
//!   mark is lifted and cube moves on to another workspace. So is a command
//!   that could not run at all (shell exit 126/127): a workspace nothing was
//!   done to is not withheld for it.
//!
//! ## Bytes freed
//!
//! Measured generically as the change in the volume's available space across
//! the command, like the compaction pass. It is an observation of the volume,
//! so concurrent writers perturb it; it answers "did this help, and roughly how
//! much".

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::config;
use crate::lock::RepoLock;
use crate::metadata::{RepoRecord, WorkspaceHealth, WorkspaceRecord, WorkspaceState};
use crate::setup::{CLEANUP_BUSY_EXIT_CODE, CleanupStep, read_cleanup_step, workspace_step_env};
use crate::store::{CleanupResult, CleanupStatus, Store, WorkspaceCleanupState, WorkspaceListFilter};

use crate::app::disk::{DiskSpace, human_bytes};
use crate::app::errors::{CubeError, Result};
use crate::app::jj::workspace_path_exists;
use crate::app::util::{cleanup_lock_path, current_epoch_s, repo_lock_path};

/// Upper bound on one cleanup command. A multi-GiB tree with read-only
/// directories takes tens of seconds to remove; this is the bound on a wedged
/// one, after which the command is killed and counted as a failure.
const CLEANUP_STEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How old a `running` mark must be before another pass treats its process as
/// dead and takes the workspace over. Comfortably above
/// [`CLEANUP_STEP_TIMEOUT`], so a live-but-slow cleanup is never taken over.
const CLEANUP_STALE_SECS: i64 = 60 * 60;

/// Minimum gap between retries of a workspace whose cleanup failed. The
/// workspace stays out of the pool meanwhile, so this is as short as is
/// polite to a command that is probably failing for a reason that persists.
const CLEANUP_RETRY_SECS: i64 = 10 * 60;

/// Longest stretch of a command's output kept in the audit record.
const AUDIT_OUTPUT_LIMIT: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CleanupTrigger {
    /// The pool GC pass that runs from `cube workspace lease`.
    Periodic,
    /// An explicit `cube workspace gc` invocation.
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CleanupReason {
    /// Free space on the volume is below the low-water mark.
    LowDisk,
    /// A previous cleanup of this workspace failed or died; finishing it does
    /// not wait for disk pressure, because the workspace is withheld from the
    /// pool until it succeeds.
    RetryFailed,
}

impl CleanupReason {
    fn describe(self) -> &'static str {
        match self {
            Self::LowDisk => "free space is below the low-water mark",
            Self::RetryFailed => "retrying an earlier cleanup that failed or died",
        }
    }
}

pub(super) struct CleanupOptions<'a> {
    pub(super) repo: Option<&'a str>,
    pub(super) workspace: Option<&'a str>,
    pub(super) trigger: CleanupTrigger,
    pub(super) dry_run: bool,
    pub(super) deadline: Option<Instant>,
}

/// A volume found below its low-water mark.
#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct VolumePressure {
    /// A workspace on the volume, to say which one this is.
    pub(super) workspace: String,
    pub(super) available_bytes: u64,
    pub(super) low_water_bytes: u64,
    pub(super) high_water_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct PlannedCleanup {
    pub(super) repo: String,
    pub(super) workspace_id: String,
    pub(super) reason: CleanupReason,
    /// Seconds since the workspace was last leased or released; `-1` if cube
    /// has never leased it.
    pub(super) idle_secs: i64,
}

impl PlannedCleanup {
    fn idle_description(&self) -> String {
        if self.idle_secs < 0 {
            "never leased by cube".to_string()
        } else {
            format!("idle {}", human_duration(self.idle_secs))
        }
    }
}

fn human_duration(secs: i64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct CleanedWorkspace {
    pub(super) repo: String,
    pub(super) workspace_id: String,
    pub(super) reason: CleanupReason,
    pub(super) bytes_freed: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct FailedCleanup {
    pub(super) repo: String,
    pub(super) workspace_id: String,
    pub(super) reason: CleanupReason,
    pub(super) error: String,
}

#[derive(Debug, Default, serde::Serialize, bon::Builder)]
#[builder(on(String, into))]
pub(super) struct CleanupReport {
    #[builder(default)]
    pub(super) dry_run: bool,
    #[builder(default)]
    pub(super) pressure: Vec<VolumePressure>,
    /// Dry run only: what a real pass would work through, in order. A real pass
    /// stops as soon as free space clears the high-water mark, which a dry run
    /// cannot know in advance.
    #[builder(default)]
    pub(super) planned: Vec<PlannedCleanup>,
    #[builder(default)]
    pub(super) cleaned: Vec<CleanedWorkspace>,
    /// Workspaces whose cleanup declined because they were in use.
    #[builder(default)]
    pub(super) busy: Vec<String>,
    #[builder(default)]
    pub(super) failed: Vec<FailedCleanup>,
    /// Workspaces another actor (a lease, another pass) claimed first.
    #[builder(default)]
    pub(super) contended: Vec<String>,
    #[builder(default)]
    pub(super) deadline_reached: bool,
}

impl CleanupReport {
    pub(super) fn bytes_freed(&self) -> u64 {
        self.cleaned.iter().map(|c| c.bytes_freed).sum()
    }

    /// Whether this pass found anything to do at all: free space was low, or an
    /// earlier cleanup was waiting on a retry.
    fn had_work(&self) -> bool {
        !(self.pressure.is_empty()
            && self.cleaned.is_empty()
            && self.busy.is_empty()
            && self.failed.is_empty()
            && self.contended.is_empty())
    }

    /// Whether this pass showed it can act, for the throttle's benefit: it
    /// cleaned something, or it stopped with candidates unexamined. A pass that
    /// walked everything it had and could do nothing has not, and a throttle
    /// that kept retrying it every few minutes would buy nothing. A pass with
    /// nothing to do (the healthy-volume common case) says nothing against the
    /// next one, so it counts as progress rather than muting the signal.
    pub(super) fn made_progress(&self) -> bool {
        !self.had_work() || !self.cleaned.is_empty() || self.deadline_reached
    }

    pub(super) fn summary(&self) -> String {
        if self.dry_run {
            if self.planned.is_empty() {
                return if self.pressure.is_empty() {
                    "Dry run: free space is above the low-water mark; no workspace cleanup needed.".to_string()
                } else {
                    "Dry run: free space is low but no free workspace is eligible for cleanup.".to_string()
                };
            }
            let mut out = format!(
                "Dry run: would clean up to {} workspace(s), in this order (least recently used first), \
                 stopping once free space clears the high-water mark:",
                self.planned.len(),
            );
            for (i, plan) in self.planned.iter().enumerate() {
                out.push_str(&format!(
                    "\n  {}. {}/{} — {}, {}",
                    i + 1,
                    plan.repo,
                    plan.workspace_id,
                    plan.reason.describe(),
                    plan.idle_description(),
                ));
            }
            return out;
        }
        if !self.had_work() {
            return "no workspace cleanup needed".to_string();
        }
        format!(
            "cleaned {} workspace(s) freeing {} ({} busy, {} failed)",
            self.cleaned.len(),
            human_bytes(self.bytes_freed()),
            self.busy.len(),
            self.failed.len(),
        )
    }
}

struct Candidate {
    record: WorkspaceRecord,
    repo: RepoRecord,
    /// `None` only for a workspace that still carries a mark although its repo
    /// no longer declares a cleanup step; such a mark is lifted, not retried.
    step: Option<CleanupStep>,
    reason: CleanupReason,
}

/// The two work lists of a pass: workspaces whose earlier cleanup has to be
/// finished whatever the disk looks like, and the pressure-driven candidates
/// ordered least recently used first.
struct Candidates {
    retries: Vec<Candidate>,
    by_lru: Vec<Candidate>,
}

fn collect_candidates(store: &Store, opts: &CleanupOptions<'_>, now_epoch_s: i64) -> Result<Candidates> {
    let repos: HashMap<String, RepoRecord> = store
        .list_repos()?
        .into_iter()
        .map(|repo| (repo.repo.clone(), repo))
        .collect();
    let states: HashMap<(String, String), WorkspaceCleanupState> = store
        .list_workspace_cleanup_states(opts.repo)?
        .into_iter()
        .map(|state| ((state.repo.clone(), state.workspace_id.clone()), state))
        .collect();
    let records = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: opts.repo,
        workspace_id: opts.workspace,
        ..Default::default()
    })?;

    let mut candidates = Candidates {
        retries: Vec::new(),
        by_lru: Vec::new(),
    };
    for record in records {
        // Leased is a hard stop, not a heuristic.
        if record.state != WorkspaceState::Free || !workspace_path_exists(&record) {
            continue;
        }
        // Dirty, conflicted and quarantined workspaces hold work a recovery
        // lease must be able to claim; the store refuses them too.
        if !matches!(record.health_status, None | Some(WorkspaceHealth::Clean)) {
            continue;
        }
        let Some(repo) = repos.get(&record.repo) else {
            continue;
        };
        let state = states.get(&(record.repo.clone(), record.workspace_id.clone()));
        let started = state.and_then(|s| s.started_at_epoch_s);
        let (reason, retry) = match state.and_then(|s| s.status) {
            Some(CleanupStatus::Failed) => {
                if started.is_some_and(|t| now_epoch_s.saturating_sub(t) < CLEANUP_RETRY_SECS) {
                    continue;
                }
                (CleanupReason::RetryFailed, true)
            }
            Some(CleanupStatus::Running) => {
                // Someone is cleaning it — unless that someone died.
                if !started.is_some_and(|t| t <= now_epoch_s.saturating_sub(CLEANUP_STALE_SECS)) {
                    continue;
                }
                (CleanupReason::RetryFailed, true)
            }
            None => {
                let cleaned = state.and_then(|s| s.last_cleaned_at_epoch_s);
                let activity = record.last_activity_at_epoch_s.unwrap_or(i64::MIN);
                if cleaned.is_some_and(|at| at >= activity) {
                    continue;
                }
                (CleanupReason::LowDisk, false)
            }
        };
        let step = match read_cleanup_step(&record.workspace_path, repo.source.as_deref()) {
            Ok(Some(step)) => Some(step),
            Ok(None) if retry => None,
            Ok(None) => continue,
            Err(e) => {
                eprintln!(
                    "cube: cleanup: {}: cannot read the repo's cleanup step, skipping: {e}",
                    record.workspace_id,
                );
                continue;
            }
        };
        let candidate = Candidate {
            record,
            repo: repo.clone(),
            step,
            reason,
        };
        if retry {
            candidates.retries.push(candidate);
        } else {
            candidates.by_lru.push(candidate);
        }
    }
    // A workspace cube has never leased sorts oldest of all.
    candidates
        .by_lru
        .sort_by_key(|c| c.record.last_activity_at_epoch_s.unwrap_or(i64::MIN));
    Ok(candidates)
}

/// Run one cleanup pass: finish any cleanup left marked from an earlier pass,
/// then, if a volume is below its low-water mark, clean the least recently used
/// eligible workspaces on it until free space clears the high-water mark.
///
/// Never returns an error: cleanup is maintenance and must not be able to fail
/// the lease or command that triggered it. Anything it cannot do is reported
/// to stderr, the audit log and the returned report.
///
/// `opts.deadline` is checked between workspaces, never inside one — a cleanup
/// command cannot be interrupted partway, so the deadline decides how many
/// start, not when they stop.
pub(super) fn run_cleanup_pass(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    opts: &CleanupOptions<'_>,
) -> CleanupReport {
    let mut report = CleanupReport::builder().dry_run(opts.dry_run).build();
    let _pass_lock = match acquire_pass_lock(database_path, opts) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("cube: cleanup: another cleanup pass is running; skipping this one");
            return report;
        }
        Err(e) => {
            eprintln!("cube: cleanup: cannot take the cleanup lock: {e}");
            return report;
        }
    };
    let now = match current_epoch_s() {
        Ok(now) => now,
        Err(e) => {
            eprintln!("cube: cleanup: cannot read the clock: {e}");
            return report;
        }
    };
    let pool = config::load_config().unwrap_or_default().pool;
    let candidates = match collect_candidates(store, opts, now) {
        Ok(candidates) => candidates,
        Err(e) => {
            eprintln!("cube: cleanup: failed to list workspaces: {e}");
            return report;
        }
    };

    for candidate in &candidates.retries {
        if deadline_reached(opts) {
            report.deadline_reached = true;
            return report;
        }
        let before = DiskSpace::probe(&candidate.record.workspace_path).ok();
        execute(runner, store, database_path, opts, candidate, before, &mut report);
    }

    // Volumes currently being relieved, so a volume that has cleared its
    // high-water mark is not re-opened for the rest of the pass by a candidate
    // that merely happens to sit on it.
    let mut relieving: HashSet<u64> = HashSet::new();
    for candidate in &candidates.by_lru {
        if deadline_reached(opts) {
            report.deadline_reached = true;
            break;
        }
        let path = &candidate.record.workspace_path;
        let (Some(volume), Ok(space)) = (volume_id(path), DiskSpace::probe(path)) else {
            // An unreadable volume is never the reason cube deletes anything.
            continue;
        };
        let low = pool.free_space_floor_bytes(space.total_bytes);
        let high = pool.cleanup_high_water_bytes(space.total_bytes);
        if relieving.contains(&volume) {
            if !space.is_below(high) {
                relieving.remove(&volume);
                continue;
            }
        } else if space.is_below(low) {
            relieving.insert(volume);
            eprintln!(
                "cube: cleanup: {} free on the volume holding {} (low-water {}, high-water {}); cleaning \
                 least recently used workspaces",
                human_bytes(space.available_bytes),
                path.display(),
                human_bytes(low),
                human_bytes(high),
            );
            report.pressure.push(VolumePressure {
                workspace: candidate.record.workspace_id.clone(),
                available_bytes: space.available_bytes,
                low_water_bytes: low,
                high_water_bytes: high,
            });
        } else {
            continue;
        }
        execute(runner, store, database_path, opts, candidate, Some(space), &mut report);
    }
    report
}

/// The lock that keeps cleanup passes from overlapping. A dry run changes
/// nothing and needs none. The periodic pass runs inside a lease and must not
/// make it wait on a cleanup command, so it yields; a manual pass waits.
fn acquire_pass_lock(database_path: Option<&Path>, opts: &CleanupOptions<'_>) -> Result<Option<Option<RepoLock>>> {
    if opts.dry_run {
        return Ok(Some(None));
    }
    let path = cleanup_lock_path(database_path)?;
    match opts.trigger {
        CleanupTrigger::Manual => Ok(Some(Some(RepoLock::acquire(&path)?))),
        CleanupTrigger::Periodic => Ok(RepoLock::try_acquire(&path)?.map(Some)),
    }
}

fn deadline_reached(opts: &CleanupOptions<'_>) -> bool {
    opts.deadline.is_some_and(|d| Instant::now() >= d)
}

/// An identifier for the volume `path` lives on.
fn volume_id(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.dev())
}

fn idle_secs(record: &WorkspaceRecord, now_epoch_s: i64) -> i64 {
    record
        .last_activity_at_epoch_s
        .map(|at| now_epoch_s.saturating_sub(at))
        .unwrap_or(-1)
}

/// Claim one candidate and run the repo's cleanup command against it.
fn execute(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    opts: &CleanupOptions<'_>,
    candidate: &Candidate,
    before: Option<DiskSpace>,
    report: &mut CleanupReport,
) {
    let record = &candidate.record;
    let now = current_epoch_s().unwrap_or(0);
    if opts.dry_run {
        report.planned.push(PlannedCleanup {
            repo: record.repo.clone(),
            workspace_id: record.workspace_id.clone(),
            reason: candidate.reason,
            idle_secs: idle_secs(record, now),
        });
        return;
    }

    match begin(store, database_path, record, now) {
        Ok(true) => {}
        Ok(false) => {
            // A lease (or another pass) got there first. Dropping it is free;
            // acting on a workspace someone just leased is not.
            report.contended.push(record.workspace_id.clone());
            return;
        }
        Err(e) => {
            eprintln!(
                "cube: cleanup: {}: could not claim it, skipping: {e}",
                record.workspace_id
            );
            return;
        }
    }

    let Some(step) = &candidate.step else {
        // The repo stopped declaring a cleanup step while this workspace was
        // marked. There is nothing to retry; lift the mark rather than withhold
        // the workspace forever.
        finish(store, record, CleanupResult::Skipped, now, now);
        eprintln!(
            "cube: cleanup: {}: its repo no longer declares a cleanup step; lifting the mark",
            record.workspace_id,
        );
        return;
    };

    let started = Instant::now();
    let outcome = runner.run_cleanup(
        &CommandInvocation {
            cwd: record.workspace_path.clone(),
            program: "sh".to_string(),
            args: vec!["-c".to_string(), step.command.clone()],
            env: cleanup_env(record, candidate.repo.source.as_deref(), step),
        },
        CLEANUP_STEP_TIMEOUT,
    );
    let duration_ms = started.elapsed().as_millis() as u64;
    let finished_at = current_epoch_s().unwrap_or(now);

    match outcome {
        Ok(output) => {
            finish(store, record, CleanupResult::Cleaned, now, finished_at);
            let bytes_freed = before
                .zip(DiskSpace::probe(&record.workspace_path).ok())
                .map(|(before, after)| after.available_bytes.saturating_sub(before.available_bytes))
                .unwrap_or(0);
            log_output(&record.workspace_id, &output);
            eprintln!(
                "cube: cleanup: cleaned {} ({:?}, {:?}): freed {}",
                record.workspace_id,
                candidate.reason,
                opts.trigger,
                human_bytes(bytes_freed),
            );
            audit_outcome(
                database_path,
                candidate,
                opts,
                "cleaned",
                bytes_freed,
                duration_ms,
                &output,
            );
            report.cleaned.push(CleanedWorkspace {
                repo: record.repo.clone(),
                workspace_id: record.workspace_id.clone(),
                reason: candidate.reason,
                bytes_freed,
            });
        }
        Err(CubeError::CommandFailed {
            status: Some(CLEANUP_BUSY_EXIT_CODE),
            stderr,
            ..
        }) => {
            finish(store, record, CleanupResult::Skipped, now, finished_at);
            eprintln!(
                "cube: cleanup: {} is busy, leaving it alone{}",
                record.workspace_id,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                },
            );
            audit_outcome(database_path, candidate, opts, "busy", 0, duration_ms, &stderr);
            report.busy.push(record.workspace_id.clone());
        }
        Err(e) => {
            // Left marked: not leasable until a retry succeeds.
            finish(store, record, CleanupResult::Failed, now, finished_at);
            let error = match &e {
                CubeError::CommandFailed { stderr, status, .. } if !stderr.is_empty() => {
                    format!(
                        "exit {}: {stderr}",
                        status.map_or("signal".to_string(), |s| s.to_string())
                    )
                }
                other => other.to_string(),
            };
            eprintln!(
                "cube: cleanup: {} failed, withholding it from leases until a retry succeeds: {error}",
                record.workspace_id,
            );
            audit_outcome(database_path, candidate, opts, "failed", 0, duration_ms, &error);
            report.failed.push(FailedCleanup {
                repo: record.repo.clone(),
                workspace_id: record.workspace_id.clone(),
                reason: candidate.reason,
                error,
            });
        }
    }
}

/// Take the registry claim on a workspace. Held under the repo lock — the same
/// lock the pool trim holds while it re-checks and removes a row — but only for
/// the one statement; the command itself runs unlocked.
fn begin(store: &Store, database_path: Option<&Path>, record: &WorkspaceRecord, now: i64) -> Result<bool> {
    let _lock = RepoLock::acquire(&repo_lock_path(&record.repo, database_path)?)?;
    store.begin_workspace_cleanup(
        &record.repo,
        &record.workspace_id,
        now,
        now.saturating_sub(CLEANUP_STALE_SECS),
    )
}

/// The step's environment: the workspace being cleaned, and the checkout the
/// step was declared in, which can differ (see [`CleanupStep`]).
fn cleanup_env(record: &WorkspaceRecord, source: Option<&Path>, step: &CleanupStep) -> Vec<(String, String)> {
    let mut env = workspace_step_env(&record.workspace_path, source);
    let root = step.root.as_deref().unwrap_or(&record.workspace_path);
    env.push(("CUBE_CLEANUP_ROOT".to_string(), root.display().to_string()));
    env
}

/// Record how the attempt that began at `attempt` ended.
fn finish(store: &Store, record: &WorkspaceRecord, result: CleanupResult, attempt: i64, now: i64) {
    if let Err(e) = store.finish_workspace_cleanup(&record.repo, &record.workspace_id, result, attempt, now) {
        // The mark stays `running` and is taken over once stale: the safe
        // direction to fail, since the workspace stays out of the pool.
        eprintln!(
            "cube: cleanup: {}: could not record the outcome ({result:?}): {e}",
            record.workspace_id,
        );
    }
}

fn log_output(workspace_id: &str, output: &str) {
    for line in output.lines().map(str::trim).filter(|line| !line.is_empty()) {
        eprintln!("cube: cleanup: {workspace_id}: {line}");
    }
}

fn audit_outcome(
    database_path: Option<&Path>,
    candidate: &Candidate,
    opts: &CleanupOptions<'_>,
    outcome: &str,
    bytes_freed: u64,
    duration_ms: u64,
    output: &str,
) {
    let output: String = output.trim().chars().take(AUDIT_OUTPUT_LIMIT).collect();
    crate::audit!(
        database_path,
        "workspace.cleanup",
        repo = candidate.record.repo,
        workspace_id = candidate.record.workspace_id,
        workspace_path = candidate.record.workspace_path.display().to_string(),
        outcome = outcome,
        reason = candidate.reason,
        trigger = opts.trigger,
        bytes_freed = bytes_freed,
        duration_ms = duration_ms,
        output = output,
    );
}

/// Whether any repo's workspace volume is currently below the low-water mark.
/// Cheap (one `statvfs` per repo), so the pool-GC throttle can ask it on every
/// lease.
pub(super) fn any_volume_below_low_water(store: &Store) -> bool {
    let pool = config::load_config().unwrap_or_default().pool;
    let Ok(repos) = store.list_repos() else {
        return false;
    };
    repos.iter().any(|repo| {
        DiskSpace::probe(&repo.workspace_root)
            .is_ok_and(|space| space.is_below(pool.free_space_floor_bytes(space.total_bytes)))
    })
}

/// Whether any workspace is withheld from leases by a failed (or dead)
/// cleanup. DB-only.
pub(super) fn any_cleanup_awaiting_retry(store: &Store) -> bool {
    store
        .list_workspace_cleanup_states(None)
        .is_ok_and(|states| states.iter().any(|state| state.status.is_some()))
}
