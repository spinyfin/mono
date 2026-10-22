//! Lease-time reclaim of a free workspace the registry holds as `dirty` or
//! `conflicted`, so a lease reuses it instead of minting a new workspace.
//!
//! ## Why this exists
//!
//! A lease only ever used a free workspace whose cached health was clean (or
//! unknown), or a quarantined one it could verify empty. Every other free
//! workspace — released with unpushed work preserved, returned by a lease
//! that expired under a dead worker, or marked `conflicted` because a
//! bookmark in the *shared* jj store was in conflict — was skipped, and the
//! lease provisioned a fresh workspace instead. Those workspaces were still
//! real capacity: the retention GC reclaims them by exactly the procedure
//! below, but only once they have been unhealthy for the retention TTL
//! (24 h by default) and only in a pass that runs at most once a day.
//!
//! Under a burst of engine restarts the gap between "unusable for a day" and
//! "minted now" was the whole disk: on 2026-10-20/21 the `mono` pool grew from
//! 35 to 80 workspaces while 42 of them sat free-but-unhealthy, and every one
//! of the 45 mints happened with `effective_free = 0` and eight to twenty
//! cached-unhealthy workspaces available. Each fresh workspace then built
//! Bazel cold, ~16 GiB apiece.
//!
//! ## What reclaiming means, and why it is safe
//!
//! The reclaim is the retention GC's procedure, applied on demand:
//!
//! 1. Probe the workspace (`jj git fetch`, then the reuse guard's head-status
//!    read). This is the same read-only verdict the release-time guard used
//!    when it preserved the tree.
//! 2. If `@` holds work no remote has, **salvage it first** — manifest and
//!    per-commit git-format patches outside the workspace and outside the jj
//!    store (see [`crate::app::salvage`]). A salvage that cannot be proven
//!    complete is an error, and an error means this workspace is left alone.
//! 3. Clear the cached health so the lease claims the workspace and runs its
//!    ordinary fetch-and-reset. `jj new <main>@<remote>` does not abandon a
//!    non-empty `@`: the commit stays a visible head in the shared store, and
//!    in practice every preserved `@` the Boss engine leaves behind is also
//!    pinned by its `boss-recovery/exec_*` bookmark. The salvage record is
//!    the belt to that pair of braces.
//!
//! A cached-`conflicted` workspace is the cheap case. `jj status` reports
//! every conflicted bookmark in the repo, and pool workspaces share one jj
//! store, so a single conflicted `pr/<n>` bookmark marks every free workspace
//! the scan happens to probe. Those trees are empty and on main; repairing
//! the bookmarks (which the lease already does for the one conflicted
//! candidate it picks) is all they need.
//!
//! ## Bounds
//!
//! This runs inside the lease's per-repo critical section, so it is bounded
//! the same way the quarantine reclaim is: a small probe cap and a wall-clock
//! deadline threaded into every subprocess. Hitting either bound is not a
//! failure — the lease provisions instead, and records why.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use serde_json::json;

use crate::audit;
use crate::command_runner::CommandRunner;
use crate::metadata::{WorkspaceHealth, WorkspaceRecord};
use crate::store::{EffectiveState, Store, WorkspaceListFilter};

use crate::app::errors::Result;
use crate::app::health::{WorkspaceHealthOutcome, check_workspace_health, probe_workspace_reuse};
use crate::app::jj::{self, workspace_path_exists};
use crate::app::salvage::{attributed_holder, attributed_task, salvage_workspace};

/// How many cached-unhealthy workspaces one lease will try to reclaim before
/// provisioning instead.
///
/// Each attempt costs a `jj git fetch` plus a head-status read, and a
/// candidate with unpushed work also pays one `jj diff` per salvaged commit.
/// The scan needs exactly one success, and candidates are ordered so the
/// cheapest (conflicted, empty `@`) and the longest-abandoned come first;
/// three is enough to get past a couple of probe errors without turning the
/// lease into a pool-wide sweep.
pub(super) const LEASE_UNHEALTHY_RECLAIM_MAX_PROBES: usize = 3;

/// What the scan already learned about a workspace in this same lease call,
/// so the reclaim does not re-run `jj status` for it: `Some(bookmarks)` when
/// the scan found it conflicted, `None` when it found it dirty.
pub(super) type ScanProbes = HashMap<String, Option<Vec<String>>>;

/// A workspace the reclaim returned to the pool for this lease to claim.
#[derive(Debug)]
pub(super) struct ReclaimedForLease {
    pub(super) workspace_id: String,
    /// Conflicted bookmarks to repair before the reset, if any.
    pub(super) conflicted_bookmarks: Vec<String>,
}

/// Why one candidate was passed over, recorded per workspace so the
/// provisioning event can say exactly what stood between the lease and each
/// free workspace it did not use.
#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct ReclaimRejection {
    pub(super) workspace_id: String,
    pub(super) reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// The reclaim scan's outcome, for the audit trail and the lease payload.
#[derive(Debug, Default, serde::Serialize, bon::Builder)]
#[builder(on(String, into))]
pub(super) struct LeaseReclaimReport {
    /// Cached-unhealthy free workspaces that were eligible (not excluded,
    /// directory present).
    pub(super) available: usize,
    /// How many of those were actually attempted.
    pub(super) scanned: usize,
    /// The probe cap or the deadline cut the scan short before anything was
    /// reclaimed. Stopping early *because* a candidate was reclaimed is not
    /// truncation.
    pub(super) truncated: bool,
    pub(super) timed_out: bool,
    pub(super) reclaimed: Option<String>,
    pub(super) rejected: Vec<ReclaimRejection>,
}

impl LeaseReclaimReport {
    pub(super) fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

/// Order candidates so the cheapest, most clearly abandoned ones go first:
/// `conflicted` (empty tree, bookmark repair only) ahead of `dirty`, then by
/// how long each has been unhealthy, oldest first, then by id for stability.
pub(super) fn order_reclaim_candidates(records: &mut [WorkspaceRecord]) {
    records.sort_by(|a, b| {
        let rank = |r: &WorkspaceRecord| match r.health_status {
            Some(WorkspaceHealth::Conflicted) => 0,
            _ => 1,
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| {
                a.unhealthy_since_epoch_s
                    .unwrap_or(i64::MAX)
                    .cmp(&b.unhealthy_since_epoch_s.unwrap_or(i64::MAX))
            })
            .then_with(|| a.workspace_id.cmp(&b.workspace_id))
    });
}

/// Try to return one cached-`dirty` or cached-`conflicted` free workspace to
/// the pool, salvaging any unpushed work first, and hand back its id so the
/// lease claims it instead of minting.
///
/// Never fails a lease: any probe, status, or salvage error skips that
/// candidate (recorded in the report) and the caller falls through to
/// provisioning when nothing is reclaimed.
#[allow(clippy::too_many_arguments)]
pub(super) fn reclaim_unhealthy_workspace_for_lease(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    repo: &str,
    main_branch: &str,
    exclude: &[String],
    scan_probes: &ScanProbes,
    now_epoch_s: i64,
    deadline: Option<Instant>,
) -> Result<(Option<ReclaimedForLease>, LeaseReclaimReport)> {
    let mut candidates = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: Some(repo),
        effective_state: Some(EffectiveState::FreeDirty),
        ..Default::default()
    })?;
    candidates.extend(store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: Some(repo),
        effective_state: Some(EffectiveState::FreeConflicted),
        ..Default::default()
    })?);
    candidates.retain(|record| !exclude.contains(&record.workspace_id) && workspace_path_exists(record));
    order_reclaim_candidates(&mut candidates);

    let mut report = LeaseReclaimReport {
        available: candidates.len(),
        ..Default::default()
    };
    let mut reclaimed: Option<ReclaimedForLease> = None;

    for record in candidates.iter().take(LEASE_UNHEALTHY_RECLAIM_MAX_PROBES) {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            report.timed_out = true;
            break;
        }
        report.scanned += 1;
        let ws_id = record.workspace_id.as_str();
        let prior_health = record.health_status.map(|h| h.as_str()).unwrap_or("unknown");
        let age_secs = record
            .unhealthy_since_epoch_s
            .map(|since| now_epoch_s.saturating_sub(since))
            .unwrap_or(0);

        // 1. Probe: fetch, then the reuse guard's verdict on `@`.
        let status = match probe_workspace_reuse(runner, database_path, &record.workspace_path, main_branch, deadline) {
            Ok(status) => status,
            Err(error) => {
                let reason = if jj::is_time_budget_error(&error) {
                    "probe_budget_exhausted"
                } else {
                    "probe_error"
                };
                audit!(
                    database_path,
                    "workspace.lease_reclaim_skipped",
                    repo = repo,
                    workspace_id = ws_id,
                    prior_health = prior_health,
                    reason = reason,
                    error = error.to_string(),
                );
                report.rejected.push(ReclaimRejection {
                    workspace_id: ws_id.to_string(),
                    reason,
                    error: Some(error.to_string()),
                });
                continue;
            }
        };

        // 2. Conflicted bookmarks to repair, from this lease's scan when it
        //    already looked, otherwise from one `jj status` now. A tree the
        //    reset cannot make clean (stale, husk) is skipped before any
        //    salvage record is written for it.
        let conflicted_bookmarks = match scan_probes.get(ws_id) {
            Some(probed) => probed.clone().unwrap_or_default(),
            None => match check_workspace_health(runner, database_path, &record.workspace_path) {
                Ok(WorkspaceHealthOutcome::Clean) | Ok(WorkspaceHealthOutcome::DirtyWorkingCopy) => Vec::new(),
                Ok(WorkspaceHealthOutcome::ConflictedBookmarks(bookmarks)) => bookmarks,
                Ok(other) => {
                    let reason = match other {
                        WorkspaceHealthOutcome::StaleWorkingCopy(_) => "stale_working_copy",
                        WorkspaceHealthOutcome::BrokenEmpty => "broken_empty",
                        _ => unreachable!("clean, dirty and conflicted are matched above"),
                    };
                    audit!(
                        database_path,
                        "workspace.lease_reclaim_skipped",
                        repo = repo,
                        workspace_id = ws_id,
                        prior_health = prior_health,
                        reason = reason,
                    );
                    report.rejected.push(ReclaimRejection {
                        workspace_id: ws_id.to_string(),
                        reason,
                        error: None,
                    });
                    continue;
                }
                Err(error) => {
                    audit!(
                        database_path,
                        "workspace.lease_reclaim_skipped",
                        repo = repo,
                        workspace_id = ws_id,
                        prior_health = prior_health,
                        reason = "health_check_error",
                        error = error.to_string(),
                    );
                    report.rejected.push(ReclaimRejection {
                        workspace_id: ws_id.to_string(),
                        reason: "health_check_error",
                        error: Some(error.to_string()),
                    });
                    continue;
                }
            },
        };

        // 3. Salvage anything no remote has. Only a complete salvage licenses
        //    the reset; a failed one leaves the workspace exactly as it was.
        let mut salvaged_to: Option<String> = None;
        if !status.is_reusable() {
            match salvage_workspace(runner, database_path, record, main_branch, now_epoch_s, deadline) {
                Ok(path) => {
                    let path_str = path.display().to_string();
                    eprintln!("cube: lease reclaim: {ws_id} held unpushed work; salvaged to {path_str} before reuse");
                    audit!(
                        database_path,
                        "workspace.lease_reclaim_salvaged",
                        repo = repo,
                        workspace_id = ws_id,
                        prior_health = prior_health,
                        prior_holder = attributed_holder(record),
                        prior_task = attributed_task(record),
                        age_secs = age_secs,
                        head_change_id = status.head_change_id(),
                        unpushed_commits = status.unpushed_summary(),
                        salvage_path = path_str,
                    );
                    salvaged_to = Some(path_str);
                }
                Err(error) => {
                    let reason = if jj::is_time_budget_error(&error) {
                        "salvage_budget_exhausted"
                    } else {
                        "salvage_failed"
                    };
                    eprintln!("cube: lease reclaim: {ws_id}: salvage failed, leaving it retained: {error}");
                    audit!(
                        database_path,
                        "workspace.lease_reclaim_skipped",
                        repo = repo,
                        workspace_id = ws_id,
                        prior_health = prior_health,
                        reason = reason,
                        head_change_id = status.head_change_id(),
                        unpushed_commits = status.unpushed_summary(),
                        error = error.to_string(),
                    );
                    report.rejected.push(ReclaimRejection {
                        workspace_id: ws_id.to_string(),
                        reason,
                        error: Some(error.to_string()),
                    });
                    continue;
                }
            }
        }

        // 4. Hand it back to the pool. The caller claims it by id and runs the
        //    ordinary fetch-and-reset (plus bookmark repair) as for any other
        //    candidate.
        if !store.gc_clear_workspace_unhealthy_state(repo, ws_id)? {
            report.rejected.push(ReclaimRejection {
                workspace_id: ws_id.to_string(),
                reason: "no_longer_free",
                error: None,
            });
            continue;
        }
        audit!(
            database_path,
            "workspace.lease_reclaimed",
            repo = repo,
            workspace_id = ws_id,
            prior_health = prior_health,
            prior_release_reason = record.last_release_reason.as_deref(),
            prior_holder = attributed_holder(record),
            prior_task = attributed_task(record),
            unhealthy_since_epoch_s = record.unhealthy_since_epoch_s,
            age_secs = age_secs,
            reuse_reason = status.reuse_reason(),
            head_change_id = status.head_change_id(),
            unpushed_commits = status.unpushed_summary(),
            salvaged_to = salvaged_to.as_deref(),
            conflicted_bookmarks = conflicted_bookmarks,
        );
        eprintln!(
            "cube: lease reclaim: reusing {ws_id} ({prior_health} for {age_secs}s{}) instead of provisioning",
            salvaged_to
                .as_deref()
                .map(|p| format!("; salvaged to {p}"))
                .unwrap_or_default(),
        );
        report.reclaimed = Some(ws_id.to_string());
        reclaimed = Some(ReclaimedForLease {
            workspace_id: ws_id.to_string(),
            conflicted_bookmarks,
        });
        break;
    }

    report.truncated = report.reclaimed.is_none() && report.available > report.scanned;
    audit!(
        database_path,
        "workspace.lease_reclaim_scan",
        repo = repo,
        available = report.available,
        scanned = report.scanned,
        truncated = report.truncated,
        timed_out = report.timed_out,
        reclaimed = report.reclaimed.as_deref(),
        rejected = report.rejected,
    );
    Ok((reclaimed, report))
}

/// Summarise why the pool's cached-unhealthy free workspaces are unhealthy:
/// `"<health>/<last_release_reason>" -> count`, so a provisioning event can say
/// at a glance whether the pool is full of preserved work, expired leases, or
/// bookmark conflicts.
pub(super) fn unhealthy_breakdown(records: &[WorkspaceRecord]) -> serde_json::Value {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for record in records {
        let health = record.health_status.map(|h| h.as_str()).unwrap_or("unknown");
        let reason = record.last_release_reason.as_deref().unwrap_or("-");
        *counts.entry(format!("{health}/{reason}")).or_default() += 1;
    }
    json!(counts)
}
