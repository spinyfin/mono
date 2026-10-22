//! Lease-time reclaim of a free workspace the registry holds as `dirty` or
//! `conflicted`, so a lease reuses it instead of minting a new workspace.
//!
//! Before a lease provisions, it salvages any unpushed work in one free
//! dirty/conflicted workspace and resets it; a candidate that cannot be
//! salvaged is left untouched. This is the retention GC's procedure
//! ([`crate::app::gc`]), applied on demand rather than after the retention
//! TTL, and it shares that procedure's probe-and-salvage step
//! ([`probe_and_salvage`]).
//!
//! ## What reclaiming means, and why it is safe
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
//! 4. If the lease then fails before the reset completes, the reclaim is
//!    rolled back ([`ReclaimUndo`]): the salvage record is discarded and the
//!    prior cached health restored, so the next lease neither re-probes a
//!    healthy-looking row nor writes a duplicate record of the same stack.
//!
//! A cached-`conflicted` workspace is the cheap case. `jj status` reports
//! every conflicted bookmark in the repo, and pool workspaces share one jj
//! store, so a single conflicted `pr/<n>` bookmark marks every free workspace
//! the scan happens to probe. Those trees are empty and on main; repairing
//! the bookmarks (which the lease already does for the one conflicted
//! candidate it picks) is all they need.
//!
//! ## Who is not a candidate
//!
//! * Anything `--exclude`d, or with a cleanup in flight.
//! * A workspace whose lease expired in *this* lease call. The expired-holder
//!   guard in the reset refuses a non-empty `@` there (the holder may still
//!   be alive), so reclaiming it would salvage, clear, claim, be refused, and
//!   quarantine it — and mint anyway.
//! * A workspace unhealthy for less than [`LEASE_RECLAIM_MIN_UNHEALTHY_SECS`].
//!   The engine keeps an orphaned worker's workspace and redispatches with
//!   `--prefer` so the work resumes in place; the grace window is longer than
//!   that redispatch latency, so an unrelated lease does not reset the tree
//!   first.
//!
//! ## Bounds
//!
//! This runs inside the lease's per-repo critical section, so it is bounded
//! the same way the quarantine reclaim is: a small probe cap and a wall-clock
//! deadline threaded into every subprocess, the health check included.
//! Hitting either bound is not a failure — the lease provisions instead, and
//! records why. The probe cap alone would let the same few permanently
//! unsalvageable rows soak up every lease's attempts, so the starting point
//! rotates across leases through a persisted per-repo cursor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::json;

use crate::audit;
use crate::command_runner::CommandRunner;
use crate::metadata::{WorkspaceHealth, WorkspaceRecord};
use crate::store::{EffectiveState, Store, WorkspaceListFilter};

use crate::app::errors::Result;
use crate::app::health::{WorkspaceHealthOutcome, check_workspace_health_within};
use crate::app::jj::{self, workspace_path_exists};
use crate::app::salvage::{
    ProbeSalvageError, ProbedAndSalvaged, attributed_holder, attributed_task, discard_salvage_record, probe_and_salvage,
};

/// How many cached-unhealthy workspaces one lease will try to reclaim before
/// provisioning instead.
///
/// Each attempt costs a `jj git fetch` plus a head-status read, and a
/// candidate with unpushed work also pays one `jj diff` per salvaged commit.
/// The scan needs exactly one success; three is enough to get past a couple
/// of probe errors without turning the lease into a pool-wide sweep. Which
/// three is decided by a rotating cursor, so a few rows that always fail do
/// not hide the rest.
pub(super) const LEASE_UNHEALTHY_RECLAIM_MAX_PROBES: usize = 3;

/// How long a workspace must have been cached unhealthy before the lease-time
/// reclaim may take it. Comfortably longer than the engine's redispatch
/// latency after a restart, so a workspace held for resume-in-place is not
/// reset by an unrelated lease first.
pub(super) const LEASE_RECLAIM_MIN_UNHEALTHY_SECS: i64 = 15 * 60;

/// `pool_metadata` key for the per-repo rotation cursor over reclaim
/// candidates.
pub(super) fn lease_reclaim_cursor_key(repo: &str) -> String {
    format!("lease_reclaim_cursor:{repo}")
}

/// What the scan already learned about a workspace in this same lease call,
/// so the reclaim does not re-run `jj status` for it: `Some(bookmarks)` when
/// the scan found it conflicted, `None` when it found it dirty.
pub(super) type ScanProbes = HashMap<String, Option<Vec<String>>>;

/// Everything needed to put a reclaimed workspace back as it was if the lease
/// fails before the reset completes.
#[derive(Debug)]
pub(super) struct ReclaimUndo {
    repo: String,
    workspace_id: String,
    prior_health: WorkspaceHealth,
    prior_unhealthy_since_epoch_s: Option<i64>,
    salvaged_to: Option<PathBuf>,
}

impl ReclaimUndo {
    /// Discard the salvage record this reclaim wrote and restore the cached
    /// health it cleared. Call after the lease has been released.
    pub(super) fn roll_back(&self, store: &Store, database_path: Option<&Path>, reason: &str) {
        if let Some(path) = &self.salvaged_to {
            discard_salvage_record(path);
            audit!(
                database_path,
                "workspace.lease_reclaim_salvage_discarded",
                repo = self.repo,
                workspace_id = self.workspace_id,
                salvage_path = path.display().to_string(),
                reason = reason,
            );
        }
        match store.restore_workspace_unhealthy_state(
            &self.repo,
            &self.workspace_id,
            self.prior_health,
            self.prior_unhealthy_since_epoch_s,
        ) {
            Ok(_) => audit!(
                database_path,
                "workspace.lease_reclaim_rolled_back",
                repo = self.repo,
                workspace_id = self.workspace_id,
                restored_health = self.prior_health.as_str(),
                reason = reason,
            ),
            Err(error) => eprintln!(
                "warning: cube could not restore the cached `{}` health of {} after a failed lease: {error}",
                self.prior_health.as_str(),
                self.workspace_id,
            ),
        }
    }
}

/// A workspace the reclaim returned to the pool for this lease to claim.
#[derive(Debug)]
pub(super) struct ReclaimedForLease {
    pub(super) workspace_id: String,
    /// Conflicted bookmarks to repair before the reset, if any.
    pub(super) conflicted_bookmarks: Vec<String>,
    pub(super) undo: ReclaimUndo,
}

/// Why one free workspace was passed over, recorded per workspace so the
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
    /// not expired in this call, directory present, past the grace window).
    pub(super) available: usize,
    /// How many of those were actually attempted.
    pub(super) scanned: usize,
    /// The probe cap or the deadline cut the scan short before anything was
    /// reclaimed. Stopping early *because* a candidate was reclaimed is not
    /// truncation.
    pub(super) truncated: bool,
    pub(super) timed_out: bool,
    pub(super) reclaimed: Option<String>,
    /// Every cached-unhealthy free workspace the reclaim did not take, with
    /// the reason: filtered out up front, attempted and failed, or never
    /// reached.
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

/// Record that a candidate was passed over, in the audit trail and the report.
fn record_skip(
    database_path: Option<&Path>,
    report: &mut LeaseReclaimReport,
    repo: &str,
    ws_id: &str,
    prior_health: &str,
    reason: &'static str,
    error: Option<String>,
) {
    audit!(
        database_path,
        "workspace.lease_reclaim_skipped",
        repo = repo,
        workspace_id = ws_id,
        prior_health = prior_health,
        reason = reason,
        error = error.as_deref(),
    );
    report.rejected.push(ReclaimRejection {
        workspace_id: ws_id.to_string(),
        reason,
        error,
    });
}

/// What a lease hands the reclaim besides the repo: who must be left alone
/// and how much time there is.
pub(super) struct ReclaimLimits<'a> {
    /// `--exclude`d ids.
    pub(super) exclude: &'a [String],
    /// Ids whose lease expired in this same lease call.
    pub(super) expired_this_call: &'a [String],
    pub(super) now_epoch_s: i64,
    pub(super) deadline: Option<Instant>,
}

/// Try to return one cached-`dirty` or cached-`conflicted` free workspace to
/// the pool, salvaging any unpushed work first, and hand back its id so the
/// lease claims it instead of minting.
///
/// Never fails a lease: any probe, status, or salvage error skips that
/// candidate (recorded in the report) and the caller falls through to
/// provisioning when nothing is reclaimed.
pub(super) fn reclaim_unhealthy_workspace_for_lease(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    repo: &str,
    main_branch: &str,
    scan_probes: &ScanProbes,
    limits: &ReclaimLimits<'_>,
) -> Result<(Option<ReclaimedForLease>, LeaseReclaimReport)> {
    let ReclaimLimits {
        exclude,
        expired_this_call,
        now_epoch_s,
        deadline,
    } = *limits;
    let mut all = Vec::new();
    for state in [EffectiveState::FreeDirty, EffectiveState::FreeConflicted] {
        all.extend(store.list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some(repo),
            effective_state: Some(state),
            exclude_cleanup_marked: true,
            ..Default::default()
        })?);
    }

    let mut report = LeaseReclaimReport::default();
    let mut candidates = Vec::new();
    for record in all {
        let ws_id = record.workspace_id.as_str();
        let age_secs = record
            .unhealthy_since_epoch_s
            .map(|since| now_epoch_s.saturating_sub(since));
        let filtered = if exclude.iter().any(|e| e == ws_id) {
            Some("excluded")
        } else if expired_this_call.iter().any(|e| e == ws_id) {
            Some("lease_expired_this_call")
        } else if !workspace_path_exists(&record) {
            Some("directory_missing")
        } else if age_secs.is_some_and(|age| age < LEASE_RECLAIM_MIN_UNHEALTHY_SECS) {
            Some("within_grace")
        } else {
            None
        };
        match filtered {
            Some(reason) => report.rejected.push(ReclaimRejection {
                workspace_id: ws_id.to_string(),
                reason,
                error: None,
            }),
            None => {
                candidates.push(record);
            }
        }
    }
    order_reclaim_candidates(&mut candidates);
    report.available = candidates.len();

    // Rotate which candidates this lease tries, so rows that fail every time
    // cannot occupy all of the probes forever.
    let cursor_key = lease_reclaim_cursor_key(repo);
    let start = if candidates.is_empty() {
        0
    } else {
        store
            .get_pool_metadata_i(&cursor_key)
            .unwrap_or(None)
            .unwrap_or(0)
            .rem_euclid(candidates.len() as i64) as usize
    };
    let attempt_order: Vec<usize> = (0..candidates.len()).map(|i| (start + i) % candidates.len()).collect();

    let mut reclaimed: Option<ReclaimedForLease> = None;
    let mut attempted = 0usize;
    for &idx in attempt_order.iter().take(LEASE_UNHEALTHY_RECLAIM_MAX_PROBES) {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            report.timed_out = true;
            break;
        }
        let record = &candidates[idx];
        attempted += 1;
        report.scanned += 1;
        let ws_id = record.workspace_id.as_str();
        let prior_health = record.health_status.map(|h| h.as_str()).unwrap_or("unknown");
        let age_secs = record
            .unhealthy_since_epoch_s
            .map(|since| now_epoch_s.saturating_sub(since))
            .unwrap_or(0);

        // 1. Conflicted bookmarks to repair, from this lease's scan when it
        //    already looked, otherwise from one `jj status` now (bounded by
        //    the same deadline). A tree the reset cannot make clean (stale,
        //    husk) is skipped before any network or salvage work is done.
        let conflicted_bookmarks = match scan_probes.get(ws_id) {
            Some(probed) => probed.clone().unwrap_or_default(),
            None => match check_workspace_health_within(runner, database_path, &record.workspace_path, deadline) {
                Ok(WorkspaceHealthOutcome::Clean) | Ok(WorkspaceHealthOutcome::DirtyWorkingCopy) => Vec::new(),
                Ok(WorkspaceHealthOutcome::ConflictedBookmarks(bookmarks)) => bookmarks,
                Ok(WorkspaceHealthOutcome::StaleWorkingCopy(_)) => {
                    record_skip(
                        database_path,
                        &mut report,
                        repo,
                        ws_id,
                        prior_health,
                        "stale_working_copy",
                        None,
                    );
                    continue;
                }
                Ok(WorkspaceHealthOutcome::BrokenEmpty) => {
                    record_skip(
                        database_path,
                        &mut report,
                        repo,
                        ws_id,
                        prior_health,
                        "broken_empty",
                        None,
                    );
                    continue;
                }
                Err(error) => {
                    let reason = if jj::is_time_budget_error(&error) {
                        "health_check_budget_exhausted"
                    } else {
                        "health_check_error"
                    };
                    record_skip(
                        database_path,
                        &mut report,
                        repo,
                        ws_id,
                        prior_health,
                        reason,
                        Some(error.to_string()),
                    );
                    continue;
                }
            },
        };

        // 2. Probe, then salvage anything no remote has. Only a complete
        //    salvage licenses the reset; a failed one leaves the workspace
        //    exactly as it was.
        let ProbedAndSalvaged { status, salvaged_to } =
            match probe_and_salvage(runner, database_path, record, main_branch, now_epoch_s, deadline) {
                Ok(done) => done,
                Err(ProbeSalvageError::Probe(error)) => {
                    let reason = if jj::is_time_budget_error(&error) {
                        "probe_budget_exhausted"
                    } else {
                        "probe_error"
                    };
                    record_skip(
                        database_path,
                        &mut report,
                        repo,
                        ws_id,
                        prior_health,
                        reason,
                        Some(error.to_string()),
                    );
                    continue;
                }
                Err(ProbeSalvageError::Salvage { status, error }) => {
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
            };
        let salvaged_str = salvaged_to.as_ref().map(|p| p.display().to_string());
        if let Some(path_str) = &salvaged_str {
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
        }

        // 3. Hand it back to the pool. The caller claims it by id and runs the
        //    ordinary fetch-and-reset (plus bookmark repair) as for any other
        //    candidate; `undo` lets it put everything back if that fails.
        if !store.gc_clear_workspace_unhealthy_state(repo, ws_id)? {
            if let Some(path) = &salvaged_to {
                discard_salvage_record(path);
            }
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
            salvaged_to = salvaged_str.as_deref(),
            conflicted_bookmarks = conflicted_bookmarks,
        );
        eprintln!(
            "cube: lease reclaim: reusing {ws_id} ({prior_health} for {age_secs}s{}) instead of provisioning",
            salvaged_str
                .as_deref()
                .map(|p| format!("; salvaged to {p}"))
                .unwrap_or_default(),
        );
        report.reclaimed = Some(ws_id.to_string());
        reclaimed = Some(ReclaimedForLease {
            workspace_id: ws_id.to_string(),
            conflicted_bookmarks,
            undo: ReclaimUndo {
                repo: repo.to_string(),
                workspace_id: ws_id.to_string(),
                prior_health: record.health_status.unwrap_or(WorkspaceHealth::Dirty),
                prior_unhealthy_since_epoch_s: record.unhealthy_since_epoch_s,
                salvaged_to,
            },
        });
        break;
    }

    // Candidates this lease never reached say why, per workspace.
    let not_reached_reason = if report.timed_out {
        "not_attempted_deadline"
    } else {
        "not_attempted_probe_cap"
    };
    if reclaimed.is_none() {
        for &idx in attempt_order.iter().skip(attempted) {
            report.rejected.push(ReclaimRejection {
                workspace_id: candidates[idx].workspace_id.clone(),
                reason: not_reached_reason,
                error: None,
            });
        }
    }

    // Advance the rotation past what this lease tried. A reclaimed candidate
    // leaves the set, so the one after it slides into its place and the
    // cursor stops there instead of skipping it.
    if attempted > 0 {
        let advance = if reclaimed.is_some() { attempted - 1 } else { attempted };
        if let Err(error) = store.set_pool_metadata_i(&cursor_key, (start + advance) as i64) {
            eprintln!("warning: cube could not persist the lease reclaim cursor for `{repo}`: {error}");
        }
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

/// Where a lease-path mint came from, for [`report_provisioning`].
#[derive(Clone, Copy)]
pub(super) enum ProvisionPath {
    /// The scan, the quarantine reclaim and the unhealthy reclaim found
    /// nothing usable.
    NoEligibleFree,
    /// A reclaimed or chosen workspace was refused by the expired-holder
    /// reset guard after the claim, and was quarantined.
    ResetRefused,
}

impl ProvisionPath {
    fn as_str(self) -> &'static str {
        match self {
            ProvisionPath::NoEligibleFree => "no_eligible_free",
            ProvisionPath::ResetRefused => "reset_refused",
        }
    }
}

/// What a lease knew when it decided to grow the pool.
#[derive(bon::Builder)]
pub(super) struct ProvisioningContext<'a> {
    pub(super) repo: &'a str,
    pub(super) path: ProvisionPath,
    pub(super) effective_free: &'a [WorkspaceRecord],
    pub(super) cached_unhealthy: &'a [WorkspaceRecord],
    pub(super) exclude: &'a [String],
    pub(super) probes: usize,
    pub(super) scan_truncated: Option<&'static str>,
    /// This lease's own scan entries (`skipped: true` ones carry a reason).
    pub(super) health_checks: &'a [serde_json::Value],
    pub(super) reclaim: Option<&'a LeaseReclaimReport>,
    /// The workspace the reset guard refused, on [`ProvisionPath::ResetRefused`].
    pub(super) refused: Option<&'a str>,
}

/// For every free workspace the registry held when a lease minted, say why
/// the lease did not use it: excluded, a skipped scan probe, a reclaim
/// filter or failure, or never reached (probe cap or deadline).
fn free_workspace_reasons(ctx: &ProvisioningContext<'_>) -> Vec<serde_json::Value> {
    let skipped_check = |id: &str| -> Option<String> {
        ctx.health_checks
            .iter()
            .find(|check| check["workspace_id"] == json!(id) && check["skipped"] == json!(true))
            .map(|check| {
                check
                    .get("reason")
                    .unwrap_or(&check["health"])
                    .as_str()
                    .unwrap_or("skipped")
                    .to_string()
            })
    };
    let reclaim_reason = |id: &str| -> Option<&'static str> {
        ctx.reclaim
            .and_then(|r| r.rejected.iter().find(|rej| rej.workspace_id == id))
            .map(|rej| rej.reason)
    };
    let unhealthy_ids: Vec<&str> = ctx.cached_unhealthy.iter().map(|w| w.workspace_id.as_str()).collect();
    ctx.effective_free
        .iter()
        .chain(ctx.cached_unhealthy.iter())
        .map(|ws| {
            let id = ws.workspace_id.as_str();
            let scan_probe = skipped_check(id);
            let reason = if ctx.refused == Some(id) {
                "reset_refused_unpushed_work_after_expiry".to_string()
            } else if ctx.exclude.iter().any(|e| e == id) {
                "excluded".to_string()
            } else if let Some(reason) = reclaim_reason(id) {
                reason.to_string()
            } else if let Some(reason) = scan_probe.clone() {
                reason
            } else if ctx.reclaim.is_some_and(|r| r.reclaimed.as_deref() == Some(id)) {
                "reclaimed_then_lease_setup_failed".to_string()
            } else if unhealthy_ids.contains(&id) {
                format!(
                    "cached_{}_not_reclaimed",
                    ws.health_status.map(|h| h.as_str()).unwrap_or("unhealthy")
                )
            } else {
                match ctx.scan_truncated {
                    Some(why) => format!("not_probed_scan_{why}"),
                    None => "not_probed".to_string(),
                }
            };
            json!({ "workspace_id": id, "reason": reason, "scan_probe": scan_probe })
        })
        .collect()
}

/// The one place a lease-path mint is reported: a stderr line and a
/// `workspace.provisioned_no_eligible_free` event with a reason for every
/// free workspace the lease did not use. Every route to `auto_create_workspace`
/// from the lease goes through here, so no mint is unexplained.
pub(super) fn report_provisioning(database_path: Option<&Path>, ctx: &ProvisioningContext<'_>) {
    let reasons = free_workspace_reasons(ctx);
    let unhealthy = unhealthy_breakdown(ctx.cached_unhealthy);
    eprintln!(
        "cube: provisioning a new workspace for `{}` ({}): no eligible free workspace \
         (effective_free={}, cached_unhealthy={}, probed={}, passed_over={}, unhealthy={})",
        ctx.repo,
        ctx.path.as_str(),
        ctx.effective_free.len(),
        ctx.cached_unhealthy.len(),
        ctx.probes,
        reasons.len(),
        unhealthy,
    );
    audit!(
        database_path,
        "workspace.provisioned_no_eligible_free",
        repo = ctx.repo,
        path = ctx.path.as_str(),
        effective_free = ctx.effective_free.len(),
        cached_unhealthy = ctx.cached_unhealthy.len(),
        unhealthy_breakdown = unhealthy,
        probed = ctx.probes,
        scan_truncated = ctx.scan_truncated,
        free_workspaces = reasons,
        reclaim = ctx.reclaim.map(|r| r.to_json()),
    );
}
