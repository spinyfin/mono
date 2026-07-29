//! Giving disk back: compacting build-artifact trees out of free workspaces,
//! and trimming a repo's free pool down to its high-water mark.
//!
//! ## Why compaction is the primary mechanism and removal the secondary one
//!
//! The pool grew to 520 mono workspaces and ~191 GB, but the ratio is what
//! decides the fix. A pristine workspace is ~600 MB — the shared jj object
//! store means 41 mono workspaces are not 41 clones, and bazel's outputs are
//! symlinks into one shared cache — while the uncleaned Cargo `target/` trees
//! inside just 13 of 51 workspaces came to 67.1 GiB, **82% of the entire
//! workspaces tree**. The disk is not in the checkouts. It is in the build
//! output.
//!
//! And the warm checkout is the entire value of the pool: reprovisioning one
//! costs a `jj workspace add` plus setup, which is exactly what leasing is
//! supposed to avoid. So cube reclaims build artifacts first and keeps the
//! workspace immediately reusable, and only removes whole workspaces when a
//! repo holds more free ones than it has any use for.
//!
//! The trade-off compaction makes is explicit: a compacted workspace keeps its
//! checkout but loses its incremental build cache, so the next lease to land
//! there pays a cold build. That is the right way round — the checkout is the
//! expensive half to rebuild and the half cube exists to hand over — and the
//! idle window in [`crate::config::DEFAULT_COMPACT_IDLE_HOURS`] keeps it away
//! from workspaces still in active rotation.
//!
//! ## What this module will not do
//!
//! - **Never a leased workspace.** Every pass filters on
//!   [`WorkspaceState::Free`] and re-checks it immediately before acting, so a
//!   workspace claimed mid-pass is dropped rather than touched. Leases are
//!   honoured even when they are stale — see the note on orphans below.
//! - **Never the shared object stores.** Nothing here touches `repos/`
//!   except to `jj workspace forget` the registration of a workspace it has
//!   just removed, which is the exact inverse of the `jj workspace add` that
//!   created it. Skipping that would leave the shared store accumulating dead
//!   registrations whose working-copy commits keep objects alive — i.e. would
//!   stop the removal from actually reclaiming disk.
//! - **Never work that exists nowhere else.** Removal is gated on the same
//!   `probe_workspace_reuse` verdict the dirty-reclaim guard uses; a workspace
//!   holding anything no remote has is skipped, and left to the retention pass
//!   which salvages before it resets.
//!
//! ## Behaviour when many leases are stale-but-live-looking
//!
//! Orphaned leases (a worker gone but its heartbeat still refreshing the
//! lease) are a known, separately-tracked engine defect, and this module is
//! designed on the assumption they will keep happening. They degrade
//! reclamation gracefully rather than breaking it: an orphan-held workspace is
//! `leased`, so it is invisible to both passes here — its `target/` is not
//! reclaimed and it does not count toward the free high-water mark. Nothing
//! fails, and no lease is ever refused because orphans exist, because there is
//! no cap on the pool to refuse against. The cost is bounded and visible: the
//! disk those orphans hold stays held, and the new `lease.heartbeat` audit
//! event makes exactly that population greppable (a lease whose age is days
//! and whose heartbeat count is in the hundreds is an orphan). If orphans grow
//! until the volume itself approaches exhaustion, the free-space floor still
//! fires on every lease and compacts everything free that is left.

use std::fs;
use std::path::Path;
use std::time::Instant;

use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
use crate::config::PoolConfig;
use crate::metadata::{RepoRecord, WorkspaceRecord, WorkspaceState};
use crate::store::{Store, WorkspaceListFilter};
use crate::{audit, config};

use crate::app::disk::{DiskSpace, human_bytes};
use crate::app::errors::Result;
use crate::app::health::probe_workspace_reuse;
use crate::app::jj::{cleanup_workspace_logs, workspace_path_exists};

/// What one reclamation pass achieved. `available_delta_bytes` is measured as
/// the change in the volume's available space across the pass, not by walking
/// the trees removed: walking a 20 GiB `target/` to produce a number for an
/// audit line would cost more than the deletion. It is therefore an
/// observation of the volume, and can be perturbed by anything else writing
/// concurrently — accurate enough to answer "did this help, and roughly how
/// much", which is what it is for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(super) struct ReclaimReport {
    /// Workspaces that had at least one artifact tree removed.
    pub(super) workspaces_compacted: usize,
    /// Artifact directories removed across all workspaces.
    pub(super) dirs_removed: usize,
    /// Whole workspaces removed by the free-pool trim.
    pub(super) workspaces_removed: usize,
    /// Change in the volume's available space across the pass.
    pub(super) available_delta_bytes: u64,
}

impl ReclaimReport {
    pub(super) fn is_empty(&self) -> bool {
        self.workspaces_compacted == 0 && self.workspaces_removed == 0
    }

    fn merge(&mut self, other: ReclaimReport) {
        self.workspaces_compacted += other.workspaces_compacted;
        self.dirs_removed += other.dirs_removed;
        self.workspaces_removed += other.workspaces_removed;
        self.available_delta_bytes = self.available_delta_bytes.saturating_add(other.available_delta_bytes);
    }
}

/// Reject any configured artifact-dir name that is not a plain child of the
/// workspace root.
///
/// Compaction runs `remove_dir_all` on `workspace_path.join(name)`, so a name
/// carrying a separator or `..` would let a `cube.toml` edit — or a typo —
/// aim that call anywhere on the filesystem. `.jj` and `.git` are refused for
/// the same reason they would be catastrophic: they are the workspace, not its
/// output.
pub(super) fn is_safe_artifact_dir_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name == ".jj" || name == ".git" {
        return false;
    }
    if name.contains('/') || name.contains('\\') {
        return false;
    }
    // A single normal path component and nothing else.
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(std::path::Component::Normal(_))) && components.next().is_none()
}

/// Whether `dir_name` inside `workspace_path` holds files jj is tracking.
///
/// `None` means the question could not be answered (jj failed, the working
/// copy is stale, the workspace has no store) — which callers treat as "do not
/// touch", because an unanswered safety question is not a yes.
///
/// `--ignore-working-copy` is essential rather than an optimisation: every
/// other jj command snapshots the working copy first, and if the repo did NOT
/// ignore this directory that snapshot would pull an entire build tree into
/// `@` — the precise outcome this check exists to detect, caused by running
/// the check. Reading the last snapshot answers the question without creating
/// the problem.
///
/// Deliberately calls the runner directly rather than going through `run_jj`.
/// That wrapper auto-recovers a stale working copy by running `jj workspace
/// update-stale`, which is right for the paths that are about to use the
/// workspace and wrong here: this is a read taken to decide whether deleting
/// is safe, and it must not mutate the thing it is inspecting. A stale
/// workspace simply yields `None` — unknown, therefore untouched.
fn artifact_dir_is_tracked(runner: &dyn CommandRunner, workspace_path: &Path, dir_name: &str) -> Option<bool> {
    let output = runner
        .run(&RealCommandRunner::invocation(
            workspace_path,
            "jj",
            &["--ignore-working-copy", "file", "list", "-r", "@", dir_name],
        ))
        .ok()?;
    Some(!output.trim().is_empty())
}

/// Remove one workspace's build-artifact trees. Returns the directories
/// removed.
///
/// Every candidate must clear four gates before `remove_dir_all` sees it: the
/// configured name is a plain child (`is_safe_artifact_dir_name`), the entry
/// exists and is a real directory, it is **not a symlink**, and jj is not
/// tracking anything inside it.
///
/// The symlink gate is the one that matters most here. Bazel puts `bazel-out`,
/// `bazel-bin` and friends in every workspace as symlinks into a single shared
/// output base under `~/Library/Caches/bazel/`. They occupy no meaningful
/// space in the workspace tree, so there is nothing to gain by removing them —
/// and following one would delete through it into the output base every other
/// workspace on the machine shares.
fn compact_workspace(runner: &dyn CommandRunner, workspace_path: &Path, artifact_dirs: &[String]) -> Vec<String> {
    let mut removed = Vec::new();
    for dir_name in artifact_dirs {
        if !is_safe_artifact_dir_name(dir_name) {
            eprintln!(
                "cube: compaction: refusing to reclaim `{dir_name}`: only a plain child directory of \
                 the workspace root is eligible (and never .jj/.git)"
            );
            continue;
        }
        let candidate = workspace_path.join(dir_name);
        // symlink_metadata, not metadata: `metadata` follows the link and
        // would report a bazel output symlink as the directory it points at.
        let Ok(meta) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        match artifact_dir_is_tracked(runner, workspace_path, dir_name) {
            Some(false) => {}
            Some(true) => {
                eprintln!(
                    "cube: compaction: {} holds files jj is tracking; leaving it alone",
                    candidate.display(),
                );
                continue;
            }
            None => {
                // Could not ask. Skip rather than guess.
                continue;
            }
        }
        match fs::remove_dir_all(&candidate) {
            Ok(()) => removed.push(dir_name.clone()),
            Err(e) => eprintln!("cube: compaction: failed to remove {}: {e}", candidate.display()),
        }
    }
    removed
}

/// Reclaim build-artifact trees from every free workspace, optionally scoped
/// to one repo.
///
/// `urgent` skips the idle window: the routine pass leaves a recently-released
/// workspace's build cache alone because it is likely to be re-leased within
/// the hour, but a volume near exhaustion has already settled that trade.
///
/// Never returns an error. A workspace that cannot be compacted is reported to
/// stderr and skipped; reclamation is best-effort maintenance and must never
/// be able to fail the lease that triggered it.
pub(super) fn compact_free_workspaces(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    repo: Option<&str>,
    now_epoch_s: i64,
    urgent: bool,
    deadline: Option<Instant>,
) -> ReclaimReport {
    let pool = config::load_config().unwrap_or_default().pool;
    let artifact_dirs = pool.build_artifact_dirs();
    if artifact_dirs.is_empty() {
        return ReclaimReport::default();
    }
    // The threshold is an upper bound on "last used at", so admitting
    // everything means i64::MAX, not i64::MIN — a workspace qualifies when its
    // last activity is at or before it.
    let idle_threshold = if urgent {
        i64::MAX
    } else {
        now_epoch_s.saturating_sub(pool.compact_idle_secs())
    };

    let records = match store.list_workspaces_filtered(&WorkspaceListFilter {
        repo,
        ..Default::default()
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cube: compaction: failed to list workspaces: {e}");
            return ReclaimReport::default();
        }
    };

    let mut report = ReclaimReport::default();
    for record in &records {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("cube: compaction: time budget exhausted, deferring the rest to the next pass");
            break;
        }
        // Leased is a hard stop, not a heuristic: a live worker's build tree
        // is in use, and cube does not get to decide otherwise.
        if record.state != WorkspaceState::Free || !workspace_path_exists(record) {
            continue;
        }
        if !is_idle_enough(record, idle_threshold) {
            continue;
        }
        // Re-read state immediately before acting. The routine pass runs
        // without the repo lock (the same trade the aged-unhealthy pass
        // makes), so a lease could have claimed this row since the list above.
        if !still_free(store, record) {
            continue;
        }

        let before = DiskSpace::probe(&record.workspace_path).ok();
        let removed = compact_workspace(runner, &record.workspace_path, &artifact_dirs);
        if removed.is_empty() {
            continue;
        }
        let freed = before
            .zip(DiskSpace::probe(&record.workspace_path).ok())
            .map(|(before, after)| after.available_bytes.saturating_sub(before.available_bytes))
            .unwrap_or(0);

        report.workspaces_compacted += 1;
        report.dirs_removed += removed.len();
        report.available_delta_bytes = report.available_delta_bytes.saturating_add(freed);
        eprintln!(
            "cube: compaction: reclaimed {} from {} ({})",
            human_bytes(freed),
            record.workspace_id,
            removed.join(", "),
        );
        audit!(
            database_path,
            "workspace.compacted",
            repo = record.repo,
            workspace_id = record.workspace_id,
            dirs = removed,
            urgent = urgent,
            idle_secs = idle_secs(record, now_epoch_s),
            available_delta_bytes = freed,
        );
    }
    report
}

/// Trim each repo's free workspaces down to its high-water mark.
///
/// This is the secondary mechanism, and deliberately the weaker one. It fires
/// only when a repo holds more *free* workspaces than the mark
/// ([`PoolConfig::max_free_workspaces`]), removes only down to that mark, and
/// removes the most-idle ones first so warm capacity survives. Leased
/// workspaces are neither counted nor candidates.
///
/// Every removal is verified with the same reuse probe the dirty-reclaim guard
/// uses. A workspace holding anything no remote has is skipped and left to the
/// retention pass, which salvages it to a durable record before resetting it;
/// it becomes eligible here on a later pass, once there is provably nothing to
/// lose. That means a repo whose surplus is entirely unpushed work will not
/// reach its mark, which is correct: the mark is a disk target, not a promise
/// that outranks somebody's afternoon.
pub(super) fn trim_free_workspaces_to_mark(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    now_epoch_s: i64,
    deadline: Option<Instant>,
) -> ReclaimReport {
    let pool = config::load_config().unwrap_or_default().pool;
    let repos = match store.list_repos() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cube: pool trim: failed to list repos: {e}");
            return ReclaimReport::default();
        }
    };

    let mut report = ReclaimReport::default();
    for repo_record in &repos {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("cube: pool trim: time budget exhausted, deferring remaining repos to the next pass");
            break;
        }
        report.merge(trim_one_repo(
            runner,
            store,
            database_path,
            &pool,
            repo_record,
            now_epoch_s,
            deadline,
        ));
    }
    report
}

fn trim_one_repo(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    pool: &PoolConfig,
    repo_record: &RepoRecord,
    now_epoch_s: i64,
    deadline: Option<Instant>,
) -> ReclaimReport {
    let repo = repo_record.repo.as_str();
    let mark = pool.max_free_workspaces(repo);
    let free = free_workspaces(store, repo);
    let mut report = ReclaimReport::default();
    if free.len() <= mark {
        return report;
    }
    let surplus = free.len() - mark;

    // Most idle first. A row cube has never leased (`None`) sorts oldest of
    // all — it is the strongest evidence nobody wants the workspace, and the
    // reuse probe below still has the final say.
    let mut candidates: Vec<&WorkspaceRecord> = free.iter().filter(|r| workspace_path_exists(r)).collect();
    candidates.sort_by_key(|r| r.last_activity_at_epoch_s.unwrap_or(i64::MIN));

    let mut skipped_holding_work = 0usize;
    let mut probe_errors = 0usize;
    for record in candidates {
        if report.workspaces_removed >= surplus {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("cube: pool trim: time budget exhausted mid-repo, deferring the rest to the next pass");
            break;
        }
        if !still_free(store, record) {
            continue;
        }

        // Look before removing anything. The probe fetches first, so "does any
        // remote already have this?" is a current answer rather than a stale
        // local view, and `deadline` is threaded into the subprocess timeouts
        // so one unreachable remote cannot outlive the whole pass.
        let status = match probe_workspace_reuse(
            runner,
            database_path,
            &record.workspace_path,
            &repo_record.main_branch,
            deadline,
        ) {
            Ok(status) => status,
            Err(e) => {
                probe_errors += 1;
                eprintln!(
                    "cube: pool trim: {}: reuse probe failed, keeping it: {e}",
                    record.workspace_id,
                );
                continue;
            }
        };
        if !status.is_reusable() {
            skipped_holding_work += 1;
            audit!(
                database_path,
                "workspace.trim_skipped",
                repo = repo,
                workspace_id = record.workspace_id,
                reason = "holds_unpushed_work",
                head_change_id = status.head_change_id(),
                unpushed_commits = status.unpushed_summary(),
            );
            continue;
        }

        let context = TrimContext {
            now_epoch_s,
            mark,
            free_before: free.len(),
        };
        match remove_workspace_from_disk(runner, store, database_path, repo_record, record, context) {
            Ok(freed) => {
                report.workspaces_removed += 1;
                report.available_delta_bytes = report.available_delta_bytes.saturating_add(freed);
            }
            Err(e) => eprintln!("cube: pool trim: {}: removal failed: {e}", record.workspace_id),
        }
    }

    audit!(
        database_path,
        "pool.trim",
        repo = repo,
        free_before = free.len(),
        mark = mark,
        surplus = surplus,
        removed = report.workspaces_removed,
        skipped_holding_work = skipped_holding_work,
        probe_errors = probe_errors,
        available_delta_bytes = report.available_delta_bytes,
    );
    report
}

/// Delete a verified-empty-of-work free workspace and drop every trace of it.
///
/// Order matters. The directory goes first, because it is the only
/// irreversible step and every check that licenses it has already passed; the
/// two bookkeeping steps after it are recoverable if they fail. `jj workspace
/// forget` against the *canonical* store is what stops the shared repo
/// accumulating dead registrations — without it the removed workspace's
/// working-copy commit stays reachable and the objects behind it are never
/// collected, so the removal would free the checkout's bytes but not the
/// history's. It is the exact inverse of the `jj workspace add` in
/// `provision.rs` that created the attachment, and it is best-effort: a
/// failure here leaves cosmetic cruft in the shared store, not a broken pool.
/// The trim's view of the repo at the moment one removal is decided, carried
/// alongside the workspace being removed so the audit line can say *why* it
/// happened — "one of 4 free against a mark of 2" — rather than just that it
/// did.
#[derive(Debug, Clone, Copy)]
struct TrimContext {
    now_epoch_s: i64,
    mark: usize,
    free_before: usize,
}

fn remove_workspace_from_disk(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    repo_record: &RepoRecord,
    record: &WorkspaceRecord,
    context: TrimContext,
) -> Result<u64> {
    let before = DiskSpace::probe(&repo_record.workspace_root).ok();
    fs::remove_dir_all(&record.workspace_path).map_err(|source| crate::app::errors::CubeError::WorkspaceDirRemove {
        path: record.workspace_path.clone(),
        source,
    })?;
    let freed = before
        .zip(DiskSpace::probe(&repo_record.workspace_root).ok())
        .map(|(before, after)| after.available_bytes.saturating_sub(before.available_bytes))
        .unwrap_or(0);

    if let Some(canonical) = &repo_record.source {
        let forget = runner.run(&CommandInvocation {
            cwd: repo_record.workspace_root.clone(),
            program: "jj".to_string(),
            args: vec![
                "-R".to_string(),
                canonical.display().to_string(),
                "workspace".to_string(),
                "forget".to_string(),
                record.workspace_id.clone(),
            ],
            env: vec![],
        });
        if let Err(e) = forget {
            eprintln!(
                "warning: cube removed {} but could not forget its registration in the shared store: {e}",
                record.workspace_id,
            );
        }
    }

    store.forget_workspace(&record.repo, &record.workspace_id)?;
    if let Err(e) = cleanup_workspace_logs(&record.workspace_id) {
        eprintln!(
            "warning: failed to clean up workspace logs for {}: {e}",
            record.workspace_id
        );
    }

    eprintln!(
        "cube: pool trim: removed {} (idle {}s; {} free for {}, mark {}), reclaiming {}",
        record.workspace_id,
        idle_secs(record, context.now_epoch_s),
        context.free_before,
        record.repo,
        context.mark,
        human_bytes(freed),
    );
    audit!(
        database_path,
        "workspace.gc_removed",
        repo = record.repo,
        workspace_id = record.workspace_id,
        workspace_path = record.workspace_path.display().to_string(),
        prior_health = record.health_status.map(|h| h.as_str()),
        prior_holder = record.last_holder.as_deref(),
        prior_task = record.last_task.as_deref(),
        idle_secs = idle_secs(record, context.now_epoch_s),
        free_before = context.free_before,
        mark = context.mark,
        available_delta_bytes = freed,
    );
    Ok(freed)
}

/// A disk-pressure reading taken on the lease path, and what reclamation did
/// about it. Returned only when the volume was actually below its floor —
/// this is the record of an intervention, not a routine measurement.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub(super) struct DiskPressureRelief {
    pub(super) before: DiskSpace,
    pub(super) after: DiskSpace,
    pub(super) floor_bytes: u64,
    pub(super) report: ReclaimReport,
    /// Whether the volume cleared its floor once reclamation was done. `false`
    /// does not fail the lease: reusing an existing workspace costs no disk
    /// and is still allowed. It only means a *mint* in this same call will be
    /// refused by [`assert_mint_headroom`].
    pub(super) recovered: bool,
}

/// If the pool's volume is below its free-space floor, compact everything free
/// in `repo` and re-measure.
///
/// This is the "free space is a first-class input" path, and it is
/// reclaim-then-proceed by design: the lease is not refused for being on a
/// full disk, it is made to pay for the space it is about to use. Compaction
/// is the only thing it does — it is fast, needs no network, and touches
/// nothing but regenerable build output, which makes it safe to run on the
/// hot path. Whole-workspace trimming needs a `jj git fetch` per candidate and
/// stays in the periodic pass.
///
/// Scoped to `repo` because the caller holds that repo's lock, so a workspace
/// cannot be claimed out from under the sweep. The periodic pass covers every
/// repo.
///
/// Returns `None` when the volume is above its floor, or when the probe itself
/// fails — an unreadable `statvfs` must never be the reason a lease does
/// something drastic.
pub(super) fn relieve_disk_pressure(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    repo: &str,
    workspace_root: &Path,
    now_epoch_s: i64,
    deadline: Option<Instant>,
) -> Option<DiskPressureRelief> {
    let pool = config::load_config().unwrap_or_default().pool;
    let before = DiskSpace::probe(workspace_root).ok()?;
    let floor_bytes = pool.free_space_floor_bytes(before.total_bytes);
    if !before.is_below(floor_bytes) {
        return None;
    }

    eprintln!(
        "cube: disk pressure: {} free on the volume holding {} (floor {}); compacting free \
         workspaces for `{repo}` before continuing",
        human_bytes(before.available_bytes),
        workspace_root.display(),
        human_bytes(floor_bytes),
    );
    let report = compact_free_workspaces(runner, store, database_path, Some(repo), now_epoch_s, true, deadline);
    let after = DiskSpace::probe(workspace_root).unwrap_or(before);
    let recovered = !after.is_below(floor_bytes);

    audit!(
        database_path,
        "pool.disk_pressure",
        repo = repo,
        workspace_root = workspace_root.display().to_string(),
        floor_bytes = floor_bytes,
        total_bytes = before.total_bytes,
        available_before_bytes = before.available_bytes,
        available_after_bytes = after.available_bytes,
        shortfall_before_bytes = before.shortfall_below(floor_bytes),
        shortfall_after_bytes = after.shortfall_below(floor_bytes),
        workspaces_compacted = report.workspaces_compacted,
        dirs_removed = report.dirs_removed,
        recovered = recovered,
    );
    if !recovered {
        eprintln!(
            "cube: disk pressure: still {} free after compacting {} workspace(s) (floor {}); \
             leasing an existing workspace remains available, growing the pool does not",
            human_bytes(after.available_bytes),
            report.workspaces_compacted,
            human_bytes(floor_bytes),
        );
    }
    Some(DiskPressureRelief {
        before,
        after,
        floor_bytes,
        report,
        recovered,
    })
}

/// Refuse to mint a workspace onto a volume below its free-space floor.
///
/// Called from the one place that grows the pool, so every mint path — the
/// ordinary one and the dirty-reclaim retry — is covered by construction.
///
/// A failed probe is explicitly *not* a refusal. cube ran for its whole life
/// without knowing anything about disk, so a host whose `statvfs` cube cannot
/// read must keep working exactly as it did before rather than losing the
/// ability to lease.
pub(super) fn assert_mint_headroom(repo: &str, workspace_root: &Path) -> Result<()> {
    let pool = config::load_config().unwrap_or_default().pool;
    let Ok(space) = DiskSpace::probe(workspace_root) else {
        eprintln!(
            "warning: cube could not read free space for {}; provisioning without a disk check",
            workspace_root.display(),
        );
        return Ok(());
    };
    let floor_bytes = pool.free_space_floor_bytes(space.total_bytes);
    if !space.is_below(floor_bytes) {
        return Ok(());
    }
    Err(crate::app::errors::CubeError::InsufficientDiskSpace {
        repo: repo.to_string(),
        workspace_root: workspace_root.to_path_buf(),
        available: human_bytes(space.available_bytes),
        floor: human_bytes(floor_bytes),
        shortfall: human_bytes(space.shortfall_below(floor_bytes)),
    })
}

/// Whether any repo currently holds more free workspaces than its mark.
///
/// Read-only and DB-only, so the pool-GC throttle can ask it cheaply: a pass
/// that ran out of budget with a surplus still outstanding should retry in
/// minutes rather than sleep for a day (see `POOL_GC_BACKLOG_RETRY_SECS`).
pub(super) fn has_free_workspace_surplus(store: &Store) -> bool {
    let pool = config::load_config().unwrap_or_default().pool;
    let Ok(repos) = store.list_repos() else {
        return false;
    };
    repos
        .iter()
        .any(|r| free_workspaces(store, &r.repo).len() > pool.max_free_workspaces(&r.repo))
}

/// Every workspace of `repo` that is not leased.
///
/// All of them count toward the high-water mark, dirty and quarantined
/// included: they all occupy disk, which is the quantity the mark bounds.
/// Whether a given one may actually be *removed* is a separate question, and
/// the reuse probe answers it.
fn free_workspaces(store: &Store, repo: &str) -> Vec<WorkspaceRecord> {
    store
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some(repo),
            ..Default::default()
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.state == WorkspaceState::Free)
        .collect()
}

/// Re-read one row and confirm it is still free, immediately before acting on
/// it. Any read failure answers "no" — losing a reclamation is free, acting on
/// a workspace someone just leased is not.
fn still_free(store: &Store, record: &WorkspaceRecord) -> bool {
    store
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some(&record.repo),
            workspace_id: Some(&record.workspace_id),
            ..Default::default()
        })
        .ok()
        .and_then(|mut v| v.pop())
        .is_some_and(|r| r.state == WorkspaceState::Free)
}

fn is_idle_enough(record: &WorkspaceRecord, idle_threshold_epoch_s: i64) -> bool {
    record.last_activity_at_epoch_s.unwrap_or(i64::MIN) <= idle_threshold_epoch_s
}

fn idle_secs(record: &WorkspaceRecord, now_epoch_s: i64) -> i64 {
    record
        .last_activity_at_epoch_s
        .map(|at| now_epoch_s.saturating_sub(at))
        .unwrap_or(-1)
}
