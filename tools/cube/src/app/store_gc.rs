//! Shared-store upkeep: bound the jj operation log of a repo's shared
//! object store.
//!
//! Every cube workspace of a repo is a secondary jj workspace on one shared
//! store (`repos/<repo>`), so every operation any of them records — a
//! `jj new` on lease, a worker's snapshot, a bookmark set, a fetch — writes
//! one view file listing every bookmark in the store, and nothing ever
//! removed them. On the dev Mac that op store reached 92 GiB (136k
//! operations, 2.6 MB views) before this existed.
//!
//! The upkeep is the generic jj recipe, in two steps, run in the store's
//! primary checkout:
//!
//! 1. `jj op abandon ..<cut>`, where `<cut>` is the newest operation that
//!    ended before the keep window. Descendant operations are reparented
//!    onto the root; views are content-addressed and untouched, so the
//!    rewrite costs one small file per kept operation. Measured on jj 0.26
//!    with a second workspace idle across the abandon: its next command
//!    (status, `jj new`) proceeds normally, and `jj workspace update-stale`
//!    reports it was never stale.
//! 2. `jj util gc`, which deletes operation and view files no head reaches.
//!    By default jj only deletes files older than two weeks, and jj 0.26
//!    accepts no other threshold than `--expire now`. The two-week default
//!    is what makes a collection safe next to a jj command in flight: the
//!    abandon above rewrote every operation newer than the cut under a new
//!    id, so a command that loaded the store before it still names the old
//!    id of the head it started from, and a collection that deleted that
//!    file would leave the command's own operation with a missing parent
//!    and every later command unable to merge the heads. But a two-week
//!    lag keeps fourteen days of views on disk, so the pass collects
//!    immediately (`--expire now`) once it has observed an instant with no
//!    `jj` process on the host: every command that loaded the store before
//!    the abandon has then finished, and any command that starts later
//!    loads the rewritten head, whose files the collection keeps. If no
//!    such instant comes within [`QUIESCENCE_WAIT`], the pass falls back to
//!    the two-week default and the next daily pass tries again.
//!
//! A third, cheap step removes `refs/heads/pr/<n>` git refs that no longer
//! have a `pr/<n>` bookmark (an older jj left them behind when the
//! bookmark was forgotten); jj's next import drops them from the view.
//!
//! The periodic pool-GC pass starts the upkeep in a detached `cube repo gc`
//! child ([`spawn_detached_store_gc`]) through the injected
//! [`CommandRunner`], never by resolving the running executable itself:
//! under `cube_lib_test` the running executable is the test binary, and a
//! test binary re-run with `repo gc <repo>` runs every test those words
//! match as a name filter, each of which leases and spawns again. That is
//! the fork bomb that took down a dev Mac on 2026-10-23; the only runner
//! that knows a cube executable is the real one, and test doubles record
//! the request.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::audit;
use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
use crate::metadata::RepoRecord;
use crate::store::Store;

use crate::app::errors::{CubeError, Result};
use crate::app::jj::run_jj;

/// Operations newer than this are never abandoned. Three days keeps
/// `jj op undo` useful for anything a worker or an operator did this week
/// while bounding the log to a few thousand operations.
pub(super) const DEFAULT_KEEP: Duration = Duration::from_secs(3 * 24 * 60 * 60);
/// The periodic pass runs the upkeep for a repo at most this often.
pub(super) const STORE_GC_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// Abandoning and collecting are not interruptible mid-file and the first
/// collection of a neglected store deletes hundreds of thousands of files;
/// this bounds each of the two jj calls.
const MAINTENANCE_CMD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long a pass waits for an instant with no `jj` process on the host
/// before it settles for the two-week collection. jj commands take
/// seconds, so a quiet instant comes within a few of them even on a busy
/// pool; a pool that is never quiet for five minutes is not one to collect
/// under.
pub(super) const QUIESCENCE_WAIT: Duration = Duration::from_secs(5 * 60);
/// How often the quiescence wait looks at the process table.
const QUIESCENCE_POLL: Duration = Duration::from_millis(250);
/// Template the cut-point selection reads: one `<operation id> <end epoch>`
/// line per operation, newest first.
pub(super) const OP_LOG_TEMPLATE: &str = r#"id ++ " " ++ time.end().format("%s") ++ "\n""#;
/// Template for the local `pr/<n>` bookmarks the stale-ref step keeps.
pub(super) const PR_BOOKMARK_NAMES_TEMPLATE: &str = r#"name ++ "\n""#;

/// Pool-metadata key stamped when the periodic pass last ran for `repo`.
pub(super) fn store_gc_last_at_key(repo: &str) -> String {
    format!("last_store_gc_at:{repo}")
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
        let ps = runner.run(&RealCommandRunner::invocation(cwd, "ps", &["-axo", "comm="]))?;
        let busy = jj_processes(&ps);
        if busy.is_empty() {
            return Ok(true);
        }
        if !announced {
            eprintln!(
                "cube: store gc: waiting for {} jj process(es) to finish before collecting immediately",
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
}

impl StoreGcReport {
    pub(super) fn summary(&self) -> String {
        let verb = if self.dry_run { "would abandon" } else { "abandoned" };
        let cut = match &self.abandoned_through {
            Some(id) => format!("{verb} operations through {}", &id[..12.min(id.len())]),
            None => "nothing older than the keep window to abandon".to_owned(),
        };
        let collected = match self.collection {
            Collection::None => "",
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
/// steps. `quiescence` is how long to wait for an instant with no jj
/// process before collecting on jj's two-week threshold instead.
pub(super) fn gc_repo_store(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    record: &RepoRecord,
    keep: Duration,
    quiescence: Duration,
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
    if !dry_run {
        if let Some(cut) = &abandoned_through {
            let range = format!("..{cut}");
            runner.run_with_timeout(
                &RealCommandRunner::invocation(&source, "jj", &["--ignore-working-copy", "op", "abandon", &range]),
                MAINTENANCE_CMD_TIMEOUT,
            )?;
        }
        // Quiescence is checked after the abandon, not before: it is the
        // commands that loaded the store before the rewrite that must have
        // finished, so the observation has to come after it.
        let mut gc_args = vec!["--ignore-working-copy", "util", "gc"];
        if wait_for_jj_quiescence(runner, &source, quiescence, QUIESCENCE_POLL)? {
            gc_args.extend(["--expire", "now"]);
            collection = Collection::Immediate;
        } else {
            collection = Collection::Deferred;
        }
        runner.run_with_timeout(
            &RealCommandRunner::invocation(&source, "jj", &gc_args),
            MAINTENANCE_CMD_TIMEOUT,
        )?;
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
