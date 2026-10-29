//! Shared-store upkeep: bound the jj operation log of a repo's shared
//! object store.
//!
//! Every cube workspace of a repo is a secondary jj workspace on one shared
//! store (`repos/<repo>`), so every operation any of them records — a
//! `jj new` on lease, a worker's snapshot, a bookmark set, a fetch — writes
//! one view file listing every bookmark in the store. The log grows without
//! bound unless old operations are abandoned and their files collected.
//!
//! The upkeep is the generic jj recipe, in two steps, run in the store's
//! primary checkout:
//!
//! 1. `jj op abandon ..<cut>`, where `<cut>` is the newest operation that
//!    ended before the keep window. Descendant operations are reparented
//!    onto the root; views are content-addressed and untouched, so the
//!    rewrite costs one small file per kept operation. The rewrite gives
//!    every kept operation a new id, so a jj command that loaded the old
//!    head and commits afterwards would leave a second operation head whose
//!    only common ancestor is the root, and jj would merge the two views
//!    against an empty base (conflicting bookmarks, undoing deletions,
//!    moving a workspace's `@`). The abandon therefore runs only at an
//!    instant with no `jj` process on the host (polling outside the gate,
//!    then one bounded probe under exclusivity); when busy, the pass abandons and
//!    collects nothing and the next daily pass tries again. After the
//!    abandon the store must hold exactly one operation head; otherwise the
//!    pass stops before collecting and reports the extra heads. The heads
//!    are counted once more after the post-abandon quiet wait and before
//!    any collection, because a command that started mid-abandon loaded the
//!    old head and only commits its second head later.
//! 2. `jj util gc`, which deletes operation and view files no head reaches.
//!    By default jj only deletes files older than two weeks, and jj 0.26
//!    accepts no other threshold than `--expire now`. A collection that
//!    deleted a file a command in flight still names would leave that
//!    command's own operation with a missing parent, so the pass collects
//!    immediately (`--expire now`) only once it has observed an instant with
//!    no `jj` process *after* the abandon: every command that loaded the
//!    store before the rewrite has then finished, and any command that
//!    starts later loads the rewritten head, whose files the collection
//!    keeps. If the post-abandon polling never finds a quiet instant, the
//!    pass falls back to the two-week default. Either way it collects only
//!    when the head re-check found a single head.
//!
//! The abandon holds the exclusive host-wide [`JjGate`] from before the
//! abandon through the head check and read-only commit-index warm-up. Every
//! `jj` command cube starts
//! (lease, goto, push, fetch; see `command_runner`) holds the gate shared for
//! its run, so none of those can load the pre-rewrite head; cube commands
//! wait at most two seconds, then return a retryable maintenance error.
//! Abandon and index warm-up share a two-minute safety deadline; interrupted
//! rewrites are checked before releasing exclusivity. An unverified rewrite
//! leaves a persistent marker that rejects cube readers until repaired.
//! Post-abandon polling and collection hold only a shared gate.
//! A `jj` command that does not go through cube (a worker's own shell, the
//! engine) cannot take part, so for
//! those the process-table probes and the head checks detect a race rather
//! than prevent it. Every run records its outcome (maintained,
//! skipped, failed) in pool metadata, and `cube workspace list` reports
//! repos whose runs keep being skipped or failing.
//!
//! Each workspace's working copy records the id of the operation it last
//! updated at, and the abandon rewrites that operation under a new id
//! without remapping the record. Verified on jj 0.26.0: a workspace whose
//! recorded operation was abandoned and collected by `--expire now`, and
//! whose `@` was rewritten from another workspace in the meantime, still
//! runs its next command (`jj status`) normally and lands on the rewritten
//! `@` without `jj workspace update-stale`.
//!
//! A third, cheap step removes `refs/heads/pr/<n>` git refs that no longer
//! have a `pr/<n>` bookmark; jj's next import drops them from the view.
//!
//! The periodic pool-GC pass starts the upkeep in a detached `cube repo gc`
//! child ([`spawn_detached_store_gc`]) through the injected
//! [`CommandRunner`], never by resolving the running executable itself:
//! under `cube_lib_test` the running executable is the test binary, and a
//! test binary re-run with `repo gc <repo>` runs every test those words
//! match as a name filter, each of which leases and spawns again. The only
//! runner that knows a cube executable is the real one, and test doubles
//! record the request.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::audit;
use crate::command_runner::{CommandInvocation, CommandRunner, JJ_GATE_HELD_ENV, JjGate, RealCommandRunner};
use crate::metadata::RepoRecord;
use crate::store::Store;

use crate::app::errors::{CubeError, Result};
use crate::app::jj::run_jj;

/// Operations newer than this are never abandoned. Three days of history
/// keeps `jj op undo` useful while the log stays bounded.
pub(super) const DEFAULT_KEEP: Duration = Duration::from_secs(3 * 24 * 60 * 60);
/// The periodic pass runs the upkeep for a repo at most this often.
pub(super) const STORE_GC_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// Listing and collecting a large log can touch hundreds of thousands of
/// files. These reader operations do not exclude leases.
const MAINTENANCE_CMD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long maintenance may wait for a quiet host or the exclusive gate.
/// Quiet-host polling happens outside exclusivity.
pub(super) const QUIESCENCE_WAIT: Duration = Duration::from_secs(5 * 60);
/// How often the quiescence wait looks at the process table.
const QUIESCENCE_POLL: Duration = Duration::from_millis(250);
/// Bound the rewrite while readers are excluded, allowing a large kept DAG.
/// Timeout kills and reaps the child; heads are checked even on failure.
/// The scale regression exercises 16,800 retained operations.
const EXCLUSIVE_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Template the cut-point selection reads: one `<operation id> <end epoch>`
/// line per operation, newest first.
pub(super) const OP_LOG_TEMPLATE: &str = r#"id ++ " " ++ time.end().format("%s") ++ "\n""#;
/// Template for the local `pr/<n>` bookmarks the stale-ref step keeps.
pub(super) const PR_BOOKMARK_NAMES_TEMPLATE: &str = r#"name ++ "\n""#;

/// Pool-metadata key stamped when the periodic pass last ran for `repo`.
pub(super) fn store_gc_last_at_key(repo: &str) -> String {
    format!("last_store_gc_at:{repo}")
}

/// Pool-metadata key for the outcome of the last `cube repo gc` run for
/// `repo`; see [`StoreGcOutcome`].
pub(super) fn store_gc_outcome_key(repo: &str) -> String {
    format!("store_gc_last_outcome:{repo}")
}

/// Pool-metadata key counting `repo`'s consecutive upkeep runs that did not
/// maintain the store (skipped or failed).
pub(super) fn store_gc_failures_key(repo: &str) -> String {
    format!("store_gc_consecutive_failures:{repo}")
}

/// How the last upkeep run for a repo ended. The detached daily child has no
/// standard streams, so this is the only place a skipped or failed pass is
/// visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StoreGcOutcome {
    /// The pass ran to completion.
    Maintained,
    /// The host never went quiet, so nothing was abandoned or collected.
    Skipped,
    /// The pass failed, including a racing operation head.
    Failed,
}

impl StoreGcOutcome {
    fn code(self) -> i64 {
        match self {
            Self::Maintained => 0,
            Self::Skipped => 1,
            Self::Failed => 2,
        }
    }

    fn from_code(code: i64) -> Option<Self> {
        match code {
            0 => Some(Self::Maintained),
            1 => Some(Self::Skipped),
            2 => Some(Self::Failed),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Maintained => "maintained",
            Self::Skipped => "skipped: jj never went quiet",
            Self::Failed => "failed",
        }
    }
}

/// Record how an upkeep run ended and bump or reset the consecutive-failure
/// count.
pub(super) fn record_store_gc_outcome(store: &Store, repo: &str, outcome: StoreGcOutcome) -> Result<()> {
    store.set_pool_metadata_i(&store_gc_outcome_key(repo), outcome.code())?;
    let failures = if outcome == StoreGcOutcome::Maintained {
        0
    } else {
        store
            .get_pool_metadata_i(&store_gc_failures_key(repo))?
            .unwrap_or(0)
            .saturating_add(1)
    };
    store.set_pool_metadata_i(&store_gc_failures_key(repo), failures)
}

/// A repo whose last upkeep runs did not maintain its store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct StoreGcAttention {
    pub repo: String,
    pub last_outcome: StoreGcOutcome,
    pub consecutive_failures: i64,
}

impl StoreGcAttention {
    pub(super) fn summary(&self) -> String {
        format!(
            "Store upkeep: {} has not been maintained for {} consecutive run(s) (last: {}); the op log is growing. \
             Run `cube repo gc {}` and check the audit log (`repo.store_gc`).",
            self.repo,
            self.consecutive_failures,
            self.last_outcome.label(),
            self.repo
        )
    }
}

/// Repos in `repos` whose last upkeep run was skipped or failed.
pub(super) fn store_gc_attention(store: &Store, repos: &[String]) -> Vec<StoreGcAttention> {
    repos
        .iter()
        .filter_map(|repo| {
            let consecutive_failures = store
                .get_pool_metadata_i(&store_gc_failures_key(repo))
                .ok()
                .flatten()
                .filter(|n| *n > 0)?;
            let last_outcome = store
                .get_pool_metadata_i(&store_gc_outcome_key(repo))
                .ok()
                .flatten()
                .and_then(StoreGcOutcome::from_code)?;
            Some(StoreGcAttention {
                repo: repo.clone(),
                last_outcome,
                consecutive_failures,
            })
        })
        .collect()
}

/// The lock one upkeep run holds for its whole run, so a second one cannot
/// start alongside it. Separate from the per-repo lease lock for the same
/// reason as the cleanup lock: a collection takes minutes, and leases must
/// never wait on it (jj itself tolerates the workspaces' concurrent
/// commands).
pub(super) fn store_gc_lock_path(repo: &str, database_path: Option<&Path>) -> Result<PathBuf> {
    let data_dir = match database_path.and_then(Path::parent) {
        Some(parent) => parent.to_path_buf(),
        None => crate::paths::data_dir()?,
    };
    Ok(data_dir.join("locks").join(format!("{repo}.store-gc.lock")))
}

/// Operation and view counts and bytes under `.jj/repo/op_store`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(super) struct OpStoreSize {
    pub operations: u64,
    pub views: u64,
    pub bytes: u64,
}

fn count_dir(dir: &Path) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut files = 0;
    let mut bytes = 0;
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            files += 1;
            bytes += meta.len();
        }
    }
    (files, bytes)
}

/// Measure the op store of the jj repo whose primary checkout is `source`.
pub(super) fn measure_op_store(source: &Path) -> OpStoreSize {
    let op_store = source.join(".jj/repo/op_store");
    let (operations, op_bytes) = count_dir(&op_store.join("operations"));
    let (views, view_bytes) = count_dir(&op_store.join("views"));
    OpStoreSize {
        operations,
        views,
        bytes: op_bytes + view_bytes,
    }
}

/// Pick the operation to abandon through: the newest one that ended at or
/// before `now - keep`, never the current head. `op_log` is
/// [`OP_LOG_TEMPLATE`] output, newest first. `None` when every operation is
/// within the keep window or the log has a single entry.
pub(super) fn select_abandon_cut(op_log: &str, now_epoch_s: i64, keep: Duration) -> Option<String> {
    let cutoff = now_epoch_s.saturating_sub(i64::try_from(keep.as_secs()).unwrap_or(i64::MAX));
    op_log
        .lines()
        .enumerate()
        .skip(1)
        .filter_map(|(_, line)| {
            let (id, ended) = line.trim().split_once(' ')?;
            let ended: i64 = ended.trim().parse().ok()?;
            (ended <= cutoff && !id.is_empty()).then(|| id.to_owned())
        })
        .next()
}

/// `refs/heads/pr/<n>` git refs with no `pr/<n>` bookmark left in jj.
/// `git_refs` is `git for-each-ref --format=%(refname) refs/heads/pr/`
/// output; `bookmarks` is [`PR_BOOKMARK_NAMES_TEMPLATE`] output.
pub(super) fn stale_pr_refs(git_refs: &str, bookmarks: &str) -> Vec<String> {
    let kept: std::collections::HashSet<&str> = bookmarks.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
    git_refs
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .filter(|r| r.strip_prefix("refs/heads/").is_some_and(|name| !kept.contains(name)))
        .map(str::to_owned)
        .collect()
}

/// The `jj` processes in `ps -axo comm=` output: one entry per line whose
/// executable's file name is `jj`. macOS prints the full path, Linux the
/// bare name; both match on the file name.
pub(super) fn jj_processes(ps_output: &str) -> Vec<String> {
    ps_output
        .lines()
        .map(str::trim)
        .filter(|line| Path::new(line).file_name().is_some_and(|name| name == "jj"))
        .map(str::to_owned)
        .collect()
}

/// Number of operation heads in the store at `source`. One means every
/// workspace's operations descend from a single head.
pub(super) fn count_op_heads(source: &Path) -> Result<usize> {
    let heads = source.join(".jj/repo/op_heads/heads");
    let mut entries = std::fs::read_dir(&heads).map_err(CubeError::Io)?;
    entries.try_fold(0, |count, entry| entry.map(|_| count + 1).map_err(CubeError::Io))
}

/// Stop the pass unless the store holds exactly one operation head. A second
/// head means a jj command raced the abandon: the next jj command merges the
/// heads against an empty base, so nothing may be collected.
fn require_single_op_head(record: &RepoRecord, source: &Path, when: &str) -> Result<()> {
    let heads = count_op_heads(source)?;
    if heads == 1 {
        return Ok(());
    }
    Err(CubeError::InvalidArgument(format!(
        "repo `{}`: the store holds {heads} operation heads {when} (expected 1); \
         the rewrite is unsafe, so nothing was collected. Repair the operation heads in {} \
         before allowing commands to merge abandoned ancestry",
        record.repo,
        source.display()
    )))
}

fn gate_data_dir(database_path: Option<&Path>) -> Result<PathBuf> {
    Ok(match database_path.and_then(Path::parent) {
        Some(parent) => parent.to_path_buf(),
        None => crate::paths::data_dir()?,
    })
}

/// The exclusive [`JjGate`] for the store gate file under the data dir the
/// run resolves. `None` when cube-started jj commands never drained within
/// `wait`, which skips the abandon like a host that is never quiet.
fn jj_maintenance_gate(database_path: Option<&Path>, wait: Duration) -> Result<Option<JjGate>> {
    match JjGate::exclusive(&JjGate::path(&gate_data_dir(database_path)?), wait) {
        Ok(gate) => Ok(Some(gate)),
        Err(CubeError::InvalidArgument(message)) => {
            eprintln!("cube: store gc: {message}");
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// Settle a leftover host-wide marker while holding the exclusive gate. A
/// marker naming this store whose heads are now a single one is a rewrite
/// that was verified or never started (a killed pass), so it is lifted. More
/// heads, or a marker naming another store, stop the pass: that store's own
/// pass owns the repair, and clearing or replacing its marker would let
/// readers merge abandoned ancestry.
fn reconcile_quarantine(gate: &JjGate, record: &RepoRecord, source: &Path) -> Result<()> {
    let Some(owner) = gate.quarantine_owner()? else {
        return Ok(());
    };
    if owner != source.display().to_string() {
        return Err(CubeError::InvalidArgument(format!(
            "repo `{}`: another store ({owner}) has an unverified jj operation-head rewrite;              repair it and run `cube repo gc` for that repo before maintaining this one",
            record.repo
        )));
    }
    require_single_op_head(record, source, "under a leftover maintenance marker")?;
    gate.clear_quarantine(source)
}

/// A maintenance `jj` command. It is marked as made by the pass that holds
/// the gate, so it does not wait on its own exclusive hold.
fn gated_jj_invocation(source: &Path, args: &[&str]) -> CommandInvocation {
    let mut invocation = RealCommandRunner::invocation(source, "jj", args);
    invocation.env.push((JJ_GATE_HELD_ENV.to_owned(), "1".to_owned()));
    invocation
}

/// Wait up to `timeout` for an instant with no `jj` process on the host,
/// polling the process table every `poll`. `Ok(true)` when one was seen.
pub(super) fn wait_for_jj_quiescence(
    runner: &dyn CommandRunner,
    cwd: &Path,
    timeout: Duration,
    poll: Duration,
) -> Result<bool> {
    let started = Instant::now();
    let mut announced = false;
    loop {
        let ps = runner.run_with_timeout(
            &RealCommandRunner::invocation(cwd, "ps", &["-axo", "comm="]),
            Duration::from_secs(1),
        )?;
        let busy = jj_processes(&ps);
        if busy.is_empty() {
            return Ok(true);
        }
        if !announced {
            eprintln!(
                "cube: store gc: waiting for {} jj process(es) to finish before maintaining the op log",
                busy.len()
            );
            announced = true;
        }
        if started.elapsed() + poll >= timeout {
            return Ok(false);
        }
        std::thread::sleep(poll);
    }
}

/// How the pass collected the files its abandon left unreferenced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Collection {
    /// A dry run collects nothing.
    None,
    /// The host never went quiet before the abandon, so neither the
    /// abandon nor the collection ran.
    Skipped,
    /// `jj util gc --expire now`, after an instant with no jj process.
    Immediate,
    /// `jj util gc` with jj's two-week threshold: the host never went
    /// quiet within the wait, so files that a command in flight may still
    /// name are left for a later pass.
    Deferred,
}

#[derive(Debug, Clone, Serialize, bon::Builder)]
#[builder(on(String, into))]
pub(super) struct StoreGcReport {
    pub repo: String,
    pub source: PathBuf,
    pub dry_run: bool,
    pub keep_secs: u64,
    pub collection: Collection,
    pub before: OpStoreSize,
    pub after: OpStoreSize,
    /// The operation `jj op abandon ..<id>` was (or would be) run with.
    pub abandoned_through: Option<String>,
    pub stale_pr_refs_removed: Vec<String>,
    pub duration_ms: u128,
    /// Wall time excluding readers, including probes and the rewrite.
    pub exclusive_ms: u128,
    /// Wall time in jj's collector, which does not exclude readers.
    pub collection_ms: u128,
}

impl StoreGcReport {
    pub(super) fn summary(&self) -> String {
        let verb = if self.dry_run { "would abandon" } else { "abandoned" };
        let cut = match &self.abandoned_through {
            None if self.collection == Collection::Skipped => "no operations abandoned".to_owned(),
            Some(id) => format!("{verb} operations through {}", &id[..12.min(id.len())]),
            None => "nothing older than the keep window to abandon".to_owned(),
        };
        let collected = match self.collection {
            Collection::None => "",
            Collection::Skipped => "; skipped (jj never went quiet, nothing was abandoned or collected)",
            Collection::Immediate => "; collected immediately",
            Collection::Deferred => "; collection deferred to jj's two-week threshold (jj never went quiet)",
        };
        format!(
            "{}: {cut}{collected}; op store {} operations / {} views / {} -> {} operations / {} views / {}; {} stale pr/* git ref(s) {}; {} ms",
            self.repo,
            self.before.operations,
            self.before.views,
            crate::app::disk::human_bytes(self.before.bytes),
            self.after.operations,
            self.after.views,
            crate::app::disk::human_bytes(self.after.bytes),
            self.stale_pr_refs_removed.len(),
            if self.dry_run { "found" } else { "removed" },
            self.duration_ms,
        )
    }
}

/// Run the upkeep for one repo's shared store. See the module doc for the
/// steps. `quiescence` bounds quiet-host polling and gate acquisition;
/// process probes under that gate never wait for the host to become quiet.
pub(super) fn gc_repo_store(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    record: &RepoRecord,
    keep: Duration,
    quiescence: Duration,
    dry_run: bool,
) -> Result<StoreGcReport> {
    gc_repo_store_with_deadline(
        runner,
        database_path,
        record,
        keep,
        quiescence,
        EXCLUSIVE_COMMAND_TIMEOUT,
        dry_run,
    )
}

/// [`gc_repo_store`] with the bound on each command run under the exclusive
/// gate given explicitly; production always uses [`EXCLUSIVE_COMMAND_TIMEOUT`].
pub(super) fn gc_repo_store_with_deadline(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    record: &RepoRecord,
    keep: Duration,
    quiescence: Duration,
    exclusive_timeout: Duration,
    dry_run: bool,
) -> Result<StoreGcReport> {
    let started = Instant::now();
    let source = record.source.clone().ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "repo `{}` has no shared source checkout to maintain",
            record.repo
        ))
    })?;
    if !source.join(".jj/repo").is_dir() {
        return Err(CubeError::InvalidArgument(format!(
            "repo `{}`: {} is not a jj repository",
            record.repo,
            source.display()
        )));
    }
    let before = measure_op_store(&source);

    // A marker left by a killed pass would reject the listing below, and
    // nothing else lifts it. Settle it first, under the exclusive gate.
    if !dry_run
        && JjGate::quarantine_path(&JjGate::path(&gate_data_dir(database_path)?))
            .try_exists()
            .map_err(CubeError::Io)?
    {
        let gate = jj_maintenance_gate(database_path, quiescence)?.ok_or_else(|| {
            CubeError::InvalidArgument(format!(
                "repo `{}`: could not take the jj maintenance gate to settle a leftover marker",
                record.repo
            ))
        })?;
        reconcile_quarantine(&gate, record, &source)?;
    }

    // Listing a neglected log is itself slow (35s for 137k operations on
    // the dev Mac), so it gets the maintenance bound, not the network one.
    let op_log = runner.run_with_timeout(
        &RealCommandRunner::invocation(
            &source,
            "jj",
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "-T",
                OP_LOG_TEMPLATE,
            ],
        ),
        MAINTENANCE_CMD_TIMEOUT,
    )?;
    let now = crate::app::util::current_epoch_s()?;
    let abandoned_through = select_abandon_cut(&op_log, now, keep);
    let mut collection = Collection::None;
    let mut exclusive_started = None;
    let mut exclusive_ms = 0;
    let mut collection_ms = 0;
    let mut abandoned_through = abandoned_through;
    if !dry_run {
        // Held through the head check and commit-index warm-up:
        // cube-started jj commands wait on it, so
        // none of them can load the pre-rewrite head in between.
        let mut gate = None;
        if let Some(cut) = abandoned_through.clone() {
            // The abandon rewrites every kept operation under a new id, so
            // it runs only with the gate held and at an instant with no jj
            // process to race it. Quiet-host polling, gate acquisition and
            // the re-probe under the gate are retried within one budget,
            // releasing the gate between attempts, so lease churn or a
            // single racing jj does not skip the pass for a day.
            let budget = Instant::now();
            let mut host_quiet = true;
            while gate.is_none() && host_quiet && budget.elapsed() < quiescence.max(Duration::from_millis(1)) {
                let remaining = quiescence.saturating_sub(budget.elapsed());
                host_quiet = wait_for_jj_quiescence(runner, &source, remaining, QUIESCENCE_POLL)?;
                if !host_quiet {
                    break;
                }
                let remaining = quiescence.saturating_sub(budget.elapsed());
                let Some(candidate) = jj_maintenance_gate(database_path, remaining)? else {
                    continue;
                };
                reconcile_quarantine(&candidate, record, &source)?;
                if wait_for_jj_quiescence(runner, &source, Duration::ZERO, QUIESCENCE_POLL)? {
                    exclusive_started = Some(Instant::now());
                    gate = Some(candidate);
                }
            }
            if let Some(active_gate) = gate.as_ref() {
                let range = format!("..{cut}");
                active_gate.quarantine(&source)?;
                let abandon = runner.run_with_timeout(
                    &gated_jj_invocation(&source, &["--ignore-working-copy", "op", "abandon", &range]),
                    exclusive_timeout,
                );
                require_single_op_head(record, &source, "after the abandon")?;
                active_gate.clear_quarantine(&source)?;
                abandon?;
                // Rewritten operation IDs invalidate jj's commit-index lookup.
                // Build that read-only index before readers resume, rather than
                // making the first lease pay for it while holding the registry
                // lock. It has its own deadline, and failing it is not fatal:
                // the rewrite is verified, and the first reader rebuilds the
                // index instead.
                if let Err(error) = runner.run_with_timeout(
                    &gated_jj_invocation(
                        &source,
                        &[
                            "--ignore-working-copy",
                            "log",
                            "-r",
                            "root()",
                            "--no-graph",
                            "-T",
                            "commit_id",
                        ],
                    ),
                    exclusive_timeout,
                ) {
                    eprintln!(
                        "cube: store gc: {}: commit-index warm-up failed after a verified abandon: {error}",
                        record.repo
                    );
                }
            } else {
                gate = None;
                abandoned_through = None;
                collection = Collection::Skipped;
            }
        }
        if collection != Collection::Skipped {
            // The observation that makes `--expire now` safe has to come
            // after the abandon: it is the commands that loaded the store
            // before the rewrite that must have finished.
            let mut gc_args = vec!["--ignore-working-copy", "util", "gc"];
            exclusive_ms = exclusive_started.map_or(0, |start| start.elapsed().as_millis());
            drop(gate);
            let _reader = JjGate::shared_for_database(database_path)?;
            let quiet = wait_for_jj_quiescence(runner, &source, quiescence, QUIESCENCE_POLL)?;
            // A command that started during the abandon loaded the old head
            // and may have committed on it since; its head only shows up
            // now, so the heads are counted again after the wait and before
            // any collection, on both the immediate and the deferred path.
            require_single_op_head(record, &source, "before the collection")?;
            if quiet {
                gc_args.extend(["--expire", "now"]);
                collection = Collection::Immediate;
            } else {
                collection = Collection::Deferred;
            }
            // Collection is a reader: leases proceed, but another rewrite
            // cannot invalidate the head this collector loaded.
            let collection_started = Instant::now();
            runner.run_with_timeout(&gated_jj_invocation(&source, &gc_args), MAINTENANCE_CMD_TIMEOUT)?;
            collection_ms = collection_started.elapsed().as_millis();
        }
    }

    let mut stale_pr_refs_removed = Vec::new();
    if source.join(".git").is_dir() {
        let git_refs = runner.run(&RealCommandRunner::invocation(
            &source,
            "git",
            &["for-each-ref", "--format=%(refname)", "refs/heads/pr/"],
        ))?;
        let bookmarks = run_jj(
            runner,
            database_path,
            &RealCommandRunner::invocation(
                &source,
                "jj",
                &[
                    "--ignore-working-copy",
                    "bookmark",
                    "list",
                    "-T",
                    PR_BOOKMARK_NAMES_TEMPLATE,
                    "glob:pr/*",
                ],
            ),
        )?;
        for stale in stale_pr_refs(&git_refs, &bookmarks) {
            if !dry_run {
                runner.run(&RealCommandRunner::invocation(
                    &source,
                    "git",
                    &["update-ref", "-d", &stale],
                ))?;
            }
            stale_pr_refs_removed.push(stale);
        }
    }

    let after = if dry_run { before } else { measure_op_store(&source) };
    let report = StoreGcReport {
        repo: record.repo.clone(),
        source,
        dry_run,
        keep_secs: keep.as_secs(),
        collection,
        before,
        after,
        abandoned_through,
        stale_pr_refs_removed,
        duration_ms: started.elapsed().as_millis(),
        exclusive_ms,
        collection_ms,
    };
    audit!(
        database_path,
        "repo.store_gc",
        repo = report.repo,
        dry_run = report.dry_run,
        collection = format!("{:?}", report.collection).to_lowercase(),
        abandoned_through = report.abandoned_through,
        operations_before = report.before.operations,
        operations_after = report.after.operations,
        views_before = report.before.views,
        views_after = report.after.views,
        bytes_before = report.before.bytes,
        bytes_after = report.after.bytes,
        stale_pr_refs_removed = report.stale_pr_refs_removed.len(),
        duration_ms = report.duration_ms as u64,
        exclusive_ms = report.exclusive_ms as u64,
        collection_ms = report.collection_ms as u64,
    );
    Ok(report)
}

/// Whether the periodic pass is due for `repo`.
pub(super) fn store_gc_due(store: &Store, repo: &str, now_epoch_s: i64) -> bool {
    match store.get_pool_metadata_i(&store_gc_last_at_key(repo)) {
        Ok(Some(last)) => now_epoch_s - last >= STORE_GC_INTERVAL_SECS,
        Ok(None) => true,
        Err(_) => false,
    }
}

/// Start the upkeep for `repo` in a detached `cube repo gc` process and
/// stamp it as run. The periodic pool-GC pass is synchronous inside a
/// short-lived `cube workspace lease`, whose time budget is twenty seconds;
/// the first collection of a neglected store deletes files for minutes, so
/// it cannot run inline there. The child has closed standard streams,
/// outlives the lease call, takes the repo's store-gc lock so a second one
/// cannot run alongside it, and records its outcome in the audit log
/// (`repo.store_gc`).
///
/// The cube executable and the spawn both come from `runner`
/// ([`CommandRunner::cube_executable`], [`CommandRunner::spawn_detached`]):
/// this function never resolves or starts the running executable itself,
/// so a test double records the request and nothing is ever started under
/// test. Returns the request that was handed to the runner, when one was.
pub(super) fn spawn_detached_store_gc(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    repo: &str,
    now_epoch_s: i64,
) -> Option<CommandInvocation> {
    let exe = match runner.cube_executable() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("cube: auto gc: cannot locate the cube binary for store upkeep of {repo}: {err}");
            return None;
        }
    };
    // Stamp first: a spawn that fails is retried next interval, while a
    // slow child must not be joined by a second one from the next lease.
    if let Err(err) = store.set_pool_metadata_i(&store_gc_last_at_key(repo), now_epoch_s) {
        eprintln!("cube: auto gc: cannot stamp store upkeep for {repo}: {err}");
        return None;
    }
    // The child resolves its data dir the way this process did: an explicit
    // database path means a non-default data dir, handed down by env.
    let data_dir = database_path.and_then(Path::parent).map(Path::to_path_buf);
    let cwd = data_dir
        .clone()
        .or_else(|| crate::paths::data_dir().ok())
        .unwrap_or_else(std::env::temp_dir);
    let mut invocation = RealCommandRunner::invocation(&cwd, &exe.to_string_lossy(), &["repo", "gc", repo]);
    if let Some(data_dir) = data_dir {
        invocation
            .env
            .push(("CUBE_DATA_DIR".to_owned(), data_dir.to_string_lossy().into_owned()));
    }
    match runner.spawn_detached(&invocation) {
        Ok(pid) => eprintln!("cube: auto gc: started shared-store upkeep for {repo} in the background (pid {pid})"),
        Err(err) => eprintln!("cube: auto gc: cannot start shared-store upkeep for {repo}: {err}"),
    }
    Some(invocation)
}
