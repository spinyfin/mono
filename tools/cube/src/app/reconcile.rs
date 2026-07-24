//! Reconciling the registry with what is actually on disk — rows whose
//! directory has vanished, and stale health status on free workspaces.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;

use crate::audit;
use crate::command_runner::CommandRunner;
use crate::lock::RepoLock;
use crate::metadata::{WorkspaceHealth, WorkspaceRecord, WorkspaceState};
use crate::store::{Store, WorkspaceListFilter};

use crate::app::errors::Result;
use crate::app::health::{WorkspaceHealthOutcome, check_workspace_health};
use crate::app::jj::workspace_path_exists;
use crate::app::util::repo_lock_path;

/// Summary of a workspace row touched by the missing-directory reconciler.
/// Surfaced through `cube workspace list --json` and also fed to per-row
/// audit events so the operator has a paper trail.
#[derive(Debug, Clone, Serialize, bon::Builder)]
#[builder(on(String, into))]
pub(super) struct ReconciledRow {
    pub(super) repo: String,
    pub(super) workspace_id: String,
    pub(super) workspace_path: PathBuf,
    prior_state: WorkspaceState,
    pub(super) lease_id: Option<String>,
    pub(super) holder: Option<String>,
    pub(super) lease_expires_at_epoch_s: Option<i64>,
}

impl ReconciledRow {
    fn from_record(record: &WorkspaceRecord) -> Self {
        ReconciledRow::builder()
            .repo(record.repo.clone())
            .workspace_id(record.workspace_id.clone())
            .workspace_path(record.workspace_path.clone())
            .prior_state(record.state)
            .maybe_lease_id(record.lease_id.clone())
            .maybe_holder(record.holder.clone())
            .maybe_lease_expires_at_epoch_s(record.lease_expires_at_epoch_s)
            .build()
    }
}

/// What `reconcile_missing_workspaces` did in one pass. `removed` is rows
/// that were actually evicted from the registry (free-and-missing, plus
/// leased-and-missing rows whose lease had already expired). `held` is
/// leased rows whose directory is gone but whose lease is still within
/// its TTL — surfaced with a stderr warning and an audit event, but left
/// in place so the operator can decide whether to `force-release`.
#[derive(Debug, Default, Clone, Serialize)]
pub(super) struct ReconcileReport {
    pub(super) removed: Vec<ReconciledRow>,
    held: Vec<ReconciledRow>,
}

impl ReconcileReport {
    fn merge(&mut self, other: ReconcileReport) {
        self.removed.extend(other.removed);
        self.held.extend(other.held);
    }
}

/// Reconcile dangling registry rows whose on-disk workspace directory has
/// been deleted out from under cube — for one specific repo.
///
/// **The caller must already hold the per-repo `RepoLock`.** Use the
/// public [`reconcile_missing_workspaces`] wrapper from call sites that
/// don't already own the lock.
///
/// Decision matrix per row:
/// - `state=free`, dir missing → forget the row (a stray directory was
///   deleted manually; the registry entry is just stale).
/// - `state=leased`, dir missing, lease TTL elapsed → force-release the
///   lease and forget the row. The worker that held it presumably
///   crashed or had its workspace wiped; the lease has already aged out.
/// - `state=leased`, dir missing, lease still active → leave the row
///   alone but warn loudly. We can't know whether the holder is mid-setup
///   or genuinely dead, so we defer to the operator (who can then
///   `cube workspace force-release <id>` and re-run).
pub(super) fn reconcile_missing_workspaces_in_repo(
    store: &mut Store,
    database_path: Option<&Path>,
    repo: &str,
    now_epoch_s: i64,
) -> Result<ReconcileReport> {
    let workspaces = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: Some(repo),
        ..Default::default()
    })?;
    let mut report = ReconcileReport::default();
    for row in workspaces {
        if workspace_path_exists(&row) {
            continue;
        }
        match row.state {
            WorkspaceState::Free => {
                let summary = ReconciledRow::from_record(&row);
                store.forget_workspace(&row.repo, &row.workspace_id)?;
                eprintln!(
                    "warning: cube workspace `{}/{}` directory is missing at {}; \
                     removing the dangling registry row",
                    row.repo,
                    row.workspace_id,
                    row.workspace_path.display(),
                );
                audit!(
                    database_path,
                    "workspace.dir_missing_reconciled",
                    repo = row.repo,
                    workspace_id = row.workspace_id,
                    workspace_path = row.workspace_path.display().to_string(),
                    prior_state = row.state.as_str(),
                );
                report.removed.push(summary);
            }
            WorkspaceState::Leased => {
                // No expiry recorded → treat as still active; we have no
                // basis to evict a lease that pre-dates the TTL field.
                let lease_active = row
                    .lease_expires_at_epoch_s
                    .map(|exp| exp > now_epoch_s)
                    .unwrap_or(true);
                if lease_active {
                    eprintln!(
                        "warning: cube workspace `{}/{}` directory is missing at {} but its \
                         lease is still active (held by {}); run `cube workspace force-release \
                         {}` to reclaim",
                        row.repo,
                        row.workspace_id,
                        row.workspace_path.display(),
                        row.holder.as_deref().unwrap_or("<unknown>"),
                        row.workspace_id,
                    );
                    audit!(
                        database_path,
                        "workspace.dir_missing_held",
                        repo = row.repo,
                        workspace_id = row.workspace_id,
                        workspace_path = row.workspace_path.display().to_string(),
                        lease_id = row.lease_id,
                        holder = row.holder,
                        lease_expires_at_epoch_s = row.lease_expires_at_epoch_s,
                    );
                    report.held.push(ReconciledRow::from_record(&row));
                } else {
                    let summary = ReconciledRow::from_record(&row);
                    if let Some(lease_id) = row.lease_id.clone() {
                        let _ = store.force_release_lease(&lease_id, Some("dir_missing"))?;
                    }
                    store.forget_workspace(&row.repo, &row.workspace_id)?;
                    eprintln!(
                        "warning: cube workspace `{}/{}` directory is missing at {} and its \
                         lease has expired (was held by {}); force-releasing and removing the \
                         dangling registry row",
                        row.repo,
                        row.workspace_id,
                        row.workspace_path.display(),
                        row.holder.as_deref().unwrap_or("<unknown>"),
                    );
                    audit!(
                        database_path,
                        "workspace.dir_missing_reconciled",
                        repo = row.repo,
                        workspace_id = row.workspace_id,
                        workspace_path = row.workspace_path.display().to_string(),
                        prior_state = row.state.as_str(),
                        lease_id = row.lease_id,
                        holder = row.holder,
                    );
                    report.removed.push(summary);
                }
            }
        }
    }
    Ok(report)
}

/// Reconcile dangling registry rows across all repos (or a single repo
/// when `repo_filter` is set). Acquires the per-repo `RepoLock` for each
/// repo that has at least one drifted row, so this is safe to call from
/// commands that don't already hold a lock.
pub(super) fn reconcile_missing_workspaces(
    store: &mut Store,
    database_path: Option<&Path>,
    repo_filter: Option<&str>,
    now_epoch_s: i64,
) -> Result<ReconcileReport> {
    let workspaces = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: repo_filter,
        ..Default::default()
    })?;
    let mut repos: Vec<String> = workspaces
        .iter()
        .filter(|ws| !workspace_path_exists(ws))
        .map(|ws| ws.repo.clone())
        .collect();
    repos.sort();
    repos.dedup();

    let mut report = ReconcileReport::default();
    for repo in repos {
        let _lock = RepoLock::acquire(&repo_lock_path(&repo, database_path)?)?;
        let sub = reconcile_missing_workspaces_in_repo(store, database_path, &repo, now_epoch_s)?;
        report.merge(sub);
    }
    Ok(report)
}

/// Result of one health-reconciliation pass entry.
#[derive(Debug, Clone, Serialize)]
pub(super) struct ReconcileHealthEntry {
    pub(super) repo: String,
    pub(super) workspace_id: String,
    /// The health status recorded in the DB before this pass.
    prior_health: String,
    /// The health status found on disk (and written to DB). `None` when the
    /// workspace was skipped without a health check.
    new_health: Option<String>,
    /// Why this workspace was skipped without being fully reconciled.
    skip_reason: Option<String>,
}

/// Summary of a `reconcile_free_workspace_health` pass.
#[derive(Debug, Default, Clone, Serialize)]
pub(super) struct ReconcileHealthReport {
    /// Workspaces that were marked dirty/conflicted in the DB but are now clean
    /// on disk. The DB has been updated to reflect this.
    pub(super) promoted_to_clean: Vec<ReconcileHealthEntry>,
    /// Workspaces that are still dirty or conflicted on disk (DB refreshed).
    pub(super) still_unhealthy: Vec<ReconcileHealthEntry>,
    /// Workspaces skipped (leased, directory missing, broken-empty, or error).
    pub(super) skipped: Vec<ReconcileHealthEntry>,
}

/// Re-check on-disk health for free workspaces that are cached as dirty or
/// conflicted in the DB, and update the cache to match.
///
/// This is the primary repair path for stale health entries: a workspace reset
/// out-of-band (manual `jj new main`, crashed worker that left it clean)
/// previously stayed `free-dirty` forever because health was only refreshed on
/// the lease/release path. This function closes that gap.
///
/// Called from:
/// - `run_pool_gc_background` (synchronously, on `cube workspace lease`,
///   throttled to roughly daily — see `maybe_trigger_pool_gc`), bounded by
///   `deadline` so it cannot itself stall lease dispatch
/// - `WorkspaceCommand::Reconcile` (explicit operator command, no deadline)
/// - Indirectly: `cube workspace lease` also lazily promotes stale-dirty
///   workspaces when it finds them clean during the health-check phase.
///
/// When `dry_run` is true the DB is not modified but the report reflects what
/// would change. When `deadline` is `Some` and it has passed, the pass stops
/// before checking the next candidate and reports what it got to so far —
/// remaining candidates are picked up on the next pass.
pub(super) fn reconcile_free_workspace_health(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    repo_filter: Option<&str>,
    workspace_filter: Option<&str>,
    dry_run: bool,
    deadline: Option<Instant>,
) -> ReconcileHealthReport {
    let all = match store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: repo_filter,
        workspace_id: workspace_filter,
        ..Default::default()
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cube: health reconcile: failed to list workspaces: {e}");
            return ReconcileHealthReport::default();
        }
    };

    let candidates: Vec<WorkspaceRecord> = all
        .into_iter()
        .filter(|r| {
            r.state == WorkspaceState::Free
                && matches!(
                    r.health_status,
                    Some(WorkspaceHealth::Dirty) | Some(WorkspaceHealth::Conflicted)
                )
        })
        .collect();

    let mut report = ReconcileHealthReport::default();

    for record in candidates {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("cube: health reconcile: time budget exhausted, deferring remaining candidates to next pass");
            break;
        }
        let prior_health = record
            .health_status
            .map(|h| h.as_str())
            .unwrap_or("unknown")
            .to_string();

        if !workspace_path_exists(&record) {
            report.skipped.push(ReconcileHealthEntry {
                repo: record.repo,
                workspace_id: record.workspace_id,
                prior_health,
                new_health: None,
                skip_reason: Some("directory_missing".to_string()),
            });
            continue;
        }

        let outcome = match check_workspace_health(runner, database_path, &record.workspace_path) {
            Ok(o) => o,
            Err(e) => {
                eprintln!(
                    "cube: health reconcile: {}: health check failed: {e}",
                    record.workspace_id,
                );
                report.skipped.push(ReconcileHealthEntry {
                    repo: record.repo,
                    workspace_id: record.workspace_id,
                    prior_health,
                    new_health: None,
                    skip_reason: Some("health_check_error".to_string()),
                });
                continue;
            }
        };

        match outcome {
            WorkspaceHealthOutcome::Clean => {
                if !dry_run {
                    if let Err(e) =
                        store.update_workspace_health(&record.repo, &record.workspace_id, WorkspaceHealth::Clean)
                    {
                        eprintln!(
                            "cube: health reconcile: {}: failed to update store: {e}",
                            record.workspace_id,
                        );
                    } else {
                        audit!(
                            database_path,
                            "workspace.health_reconciled",
                            repo = record.repo,
                            workspace_id = record.workspace_id,
                            prior_health = prior_health,
                            new_health = "clean",
                        );
                    }
                }
                report.promoted_to_clean.push(ReconcileHealthEntry {
                    repo: record.repo,
                    workspace_id: record.workspace_id,
                    prior_health,
                    new_health: Some("clean".to_string()),
                    skip_reason: None,
                });
            }
            WorkspaceHealthOutcome::DirtyWorkingCopy => {
                if !dry_run {
                    // Refresh the DB entry. `update_workspace_health` preserves
                    // `unhealthy_since_epoch_s` via COALESCE, so the age clock
                    // is not reset.
                    let _ = store.update_workspace_health(&record.repo, &record.workspace_id, WorkspaceHealth::Dirty);
                }
                report.still_unhealthy.push(ReconcileHealthEntry {
                    repo: record.repo,
                    workspace_id: record.workspace_id,
                    prior_health,
                    new_health: Some("dirty".to_string()),
                    skip_reason: None,
                });
            }
            WorkspaceHealthOutcome::ConflictedBookmarks(_) => {
                if !dry_run {
                    let _ =
                        store.update_workspace_health(&record.repo, &record.workspace_id, WorkspaceHealth::Conflicted);
                }
                report.still_unhealthy.push(ReconcileHealthEntry {
                    repo: record.repo,
                    workspace_id: record.workspace_id,
                    prior_health,
                    new_health: Some("conflicted".to_string()),
                    skip_reason: None,
                });
            }
            WorkspaceHealthOutcome::BrokenEmpty => {
                // Don't re-classify broken-empty as dirty — leave the existing
                // health marker intact and report as skipped. The broken-empty
                // state requires a clone, not a health re-classification.
                report.skipped.push(ReconcileHealthEntry {
                    repo: record.repo,
                    workspace_id: record.workspace_id,
                    prior_health,
                    new_health: None,
                    skip_reason: Some("broken_empty".to_string()),
                });
            }
            WorkspaceHealthOutcome::StaleWorkingCopy(_) => {
                // The reconcile pass doesn't attempt stale recovery — that's
                // a lease-time operation. Report as skipped so the operator
                // can run `jj workspace update-stale` manually or wait for
                // the lease path to auto-recover it.
                report.skipped.push(ReconcileHealthEntry {
                    repo: record.repo,
                    workspace_id: record.workspace_id,
                    prior_health,
                    new_health: None,
                    skip_reason: Some("stale_working_copy".to_string()),
                });
            }
        }
    }

    report
}
