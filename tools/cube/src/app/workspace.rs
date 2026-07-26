//! `cube workspace` — the lease/release/list/status/remove lifecycle that
//! hands a worker a ready-to-use workspace and takes it back afterwards.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, io};

use serde_json::json;
use uuid::Uuid;

use crate::cli::WorkspaceCommand;
use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
use crate::lock::RepoLock;
use crate::metadata::{WorkspaceHealth, WorkspaceRecord, WorkspaceState};
use crate::setup::StepStatus;
use crate::store::{EffectiveState, Store, WorkspaceListFilter};
use crate::{audit, config};

use crate::app::display::{effective_state_display, format_workspace_list, human_workspace_detail};
use crate::app::errors::{CubeError, Result, RunResult};
use crate::app::excludes::ensure_boss_infra_excluded;
use crate::app::gc::{
    gc_aged_unhealthy_workspaces, gc_workspace_bookmarks, maybe_trigger_pool_gc, reclaim_quarantined_workspace,
};
use crate::app::health::{
    WorkspaceHealthOutcome, check_workspace_health, read_head_status, repair_conflicted_bookmarks,
};
use crate::app::jj::{cleanup_workspace_logs, current_workspace_commit, run_jj, workspace_path_exists};
use crate::app::provision::{
    auto_create_workspace, discover_workspaces, find_workspace_record, format_lease_message, format_setup_message,
    gc_broken_empty_workspace, run_setup_for_workspace,
};
use crate::app::reconcile::{
    reconcile_free_workspace_health, reconcile_missing_workspaces, reconcile_missing_workspaces_in_repo,
};
use crate::app::reset::{
    PRESERVED_UNPUSHED_RELEASE_REASON, PreservedWorkingCopy, ReleaseResetOutcome, reset_workspace_guarded,
    reset_workspace_on_release,
};
use crate::app::util::{current_epoch_s, holder_identity, repo_lock_path, resolve_release_lease};
use crate::app::workspace_ops::{workspace_goto, workspace_push, workspace_rebase};

/// Default lease TTL: 24 hours from acquisition.
///
/// Was 30 minutes until the 2026-07-23 crash-and-restart incident: a worker's
/// lease that had never been heartbeated (e.g. the holder died before its
/// first heartbeat pass, or the engine itself was down) expired on cube's own
/// clock, and a fresh dispatch grabbed the now-`free` workspace out from under
/// a resume-in-place execution trying to reclaim the same one — a race lost
/// by seconds. The workspace still held the prior holder's uncommitted work,
/// which then sat unused while the resumed execution rebuilt it from scratch.
///
/// Workspaces are cheap and provisioned on demand (no fixed pool); in-flight
/// work is not. A long default TTL means a dead-but-unreaped holder keeps its
/// claim — rather than an eager fresh dispatch being able to grab the
/// workspace via ordinary TTL expiry before a resume ever gets a chance —
/// while genuinely dead leases are still reclaimed promptly through paths
/// that do not depend on this TTL at all: `cube workspace force-release`
/// (admin/engine-initiated, bypasses TTL entirely) and the Boss engine's own
/// heartbeat-failure-streak + confirm-dead auto-reap (also calls
/// force-release directly, see `cube_lease_heartbeat.rs`). This TTL is only
/// the last-resort backstop for leases nothing else is watching.
const DEFAULT_LEASE_TTL_SECS: i64 = 24 * 60 * 60;

/// `last_release_reason` recorded when the dirty-reclaim guard in
/// `reset_workspace_guarded` refuses a destructive reset and the workspace
/// is quarantined instead of the lease call hard-failing. See
/// [`crate::metadata::WorkspaceHealth::Quarantined`].
pub(super) const DIRTY_RECLAIM_QUARANTINE_REASON: &str = "dirty_reclaim_quarantined";

/// Hard cap on live `jj status` probes performed by one lease's pre-claim
/// health scan, across both candidate categories.
///
/// The scan needs exactly one hit — the first clean workspace ends it — so
/// running it to exhaustion was pure waste. Before this cap, a repo whose free
/// pool had gone all-dirty spent ~17s per lease serially re-probing every free
/// workspace (measured: 106 probes at 2.1–10.0/s depending on host load, 100%
/// of them skipped for `dirty_working_copy`) and then provisioned a fresh
/// workspace anyway. Dispatch timed out at the caller's 30s bound while cube
/// was still walking a list whose answer it had already written to SQLite.
///
/// Eight is deliberately small: a workspace that fails its probe has just had
/// its cached health corrected in the registry, so the next lease's candidate
/// set is smaller. Stopping early and provisioning costs ~6s and always
/// succeeds; scanning further is a gamble on the Nth entry being clean when
/// N-1 were not.
const LEASE_HEALTH_SCAN_MAX_PROBES: usize = 8;

/// How many of [`LEASE_HEALTH_SCAN_MAX_PROBES`] one lease may spend
/// re-validating workspaces the registry *already* records as unhealthy.
///
/// This is the staleness policy for the cached `health_status`. Cached
/// verdicts are trusted by default — that is the whole point — but a
/// workspace an operator cleaned out of band must not be withheld forever, so
/// each lease re-probes a small rotating window of the cached-unhealthy set
/// (cursor persisted in `pool_metadata`, see
/// [`STALE_HEALTH_CURSOR_KEY_PREFIX`]). Every cached-unhealthy workspace is
/// therefore re-probed at least once every `ceil(N / 3)` leases; at the
/// observed N≈107 and 259 leases per audit window that is ~7 revalidations
/// per workspace per window, against 107 probes *per lease* before.
///
/// Revalidation is not the only path back: the pool GC's
/// `reconcile_free_workspace_health` pass promotes recovered workspaces
/// wholesale, `cube workspace reconcile` does it on demand, and `--prefer`
/// always probes its named target regardless of this window.
const STALE_HEALTH_REVALIDATE_PER_LEASE: usize = 3;

/// Wall-clock budget for the whole pre-claim health scan. Belt to
/// [`LEASE_HEALTH_SCAN_MAX_PROBES`]'s braces: probe *count* is a poor proxy
/// for probe *cost* on a loaded host (the same 106-workspace scan took 46s at
/// one point and 182s at another), so the scan also stops at a deadline and
/// provisions instead.
const LEASE_HEALTH_SCAN_BUDGET: Duration = Duration::from_secs(5);

/// Wall-clock budget for the quarantine-reclaim scan that runs after the
/// health scan when nothing usable was found. Its probes each include a
/// `jj git fetch`, so it is bounded in time as well as in probe count.
const QUARANTINE_RECLAIM_BUDGET: Duration = Duration::from_secs(5);

/// `pool_metadata` key prefix for the per-repo cursor that rotates which
/// cached-unhealthy workspaces get re-probed. Persisting it (rather than, say,
/// randomising) makes the revalidation interval a stated guarantee rather than
/// an expectation.
const STALE_HEALTH_CURSOR_KEY_PREFIX: &str = "stale_health_cursor:";

fn stale_health_cursor_key(repo: &str) -> String {
    format!("{STALE_HEALTH_CURSOR_KEY_PREFIX}{repo}")
}

/// Pick the rotating slice of `cached_unhealthy` this lease will re-probe, and
/// return it with the cursor value to persist for the next lease.
///
/// Split out from the lease body so the rotation guarantee ("every entry is
/// revalidated within ceil(N/K) leases") is directly testable without driving
/// a full lease.
pub(super) fn stale_revalidation_window(len: usize, cursor: i64, per_lease: usize) -> (Vec<usize>, i64) {
    if len == 0 || per_lease == 0 {
        return (Vec::new(), 0);
    }
    let start = cursor.rem_euclid(len as i64) as usize;
    let take = per_lease.min(len);
    let picked: Vec<usize> = (0..take).map(|offset| (start + offset) % len).collect();
    (picked, ((start + take) % len) as i64)
}

pub(super) fn run_workspace(
    command: WorkspaceCommand,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<RunResult> {
    let mut store = if let Some(path) = database_path {
        Store::open_at(path)?
    } else {
        Store::open_default()?
    };

    match command {
        WorkspaceCommand::Lease {
            repo,
            task,
            prefer,
            allow_dirty,
            exclude,
            release_on_setup_failure,
        } => {
            let repo_record = store
                .get_repo(&repo)?
                .ok_or_else(|| CubeError::RepoNotFound(repo.clone()))?;

            let leased_at_epoch_s = current_epoch_s()?;
            // Kick a background pool-wide gc pass at most once per 24h.
            maybe_trigger_pool_gc(&mut store, database_path, leased_at_epoch_s)?;

            let _lock = RepoLock::acquire(&repo_lock_path(&repo, database_path)?)?;
            let mut candidates = discover_workspaces(&repo_record)?;
            store.sync_workspaces(&repo, &candidates)?;
            // Sweep any leases that have already exceeded their TTL so they
            // become claimable again. Audit each reclaimed lease before
            // doing anything else so the timeline shows the prior
            // holder/task — when a worker's `@` is observed to move
            // mid-flight, this is the first thing to grep.
            let expired = store.expire_stale_leases(&repo, leased_at_epoch_s)?;
            for swept in &expired {
                audit!(
                    database_path,
                    "lease.expired_reclaimed",
                    repo = repo,
                    workspace_id = swept.workspace_id,
                    prior_lease_id = swept.lease_id,
                    prior_holder = swept.holder.as_deref(),
                    prior_task = swept.task.as_deref(),
                    prior_leased_at_epoch_s = swept.leased_at_epoch_s,
                    prior_lease_expires_at_epoch_s = swept.lease_expires_at_epoch_s,
                );
            }
            let expired_by_workspace_id: std::collections::HashMap<&str, &crate::store::ExpiredLease> =
                expired.iter().map(|e| (e.workspace_id.as_str(), e)).collect();
            // Self-heal any rows whose on-disk directory has been deleted
            // out from under cube. The repo lock is already held by this
            // lease call, so use the `_in_repo` variant that skips its own
            // locking. After expire_stale_leases above, any leased rows
            // whose lease has aged out are now `free`, and reconcile will
            // forget them too if their directory is also missing.
            reconcile_missing_workspaces_in_repo(&mut store, database_path, &repo, leased_at_epoch_s)?;

            let lease_id = Uuid::new_v4().to_string();
            let holder = holder_identity();
            let lease_expires_at = Some(leased_at_epoch_s + DEFAULT_LEASE_TTL_SECS);

            // ── Dirty-recovery phase ────────────────────────────────────────
            // `--allow-dirty` (always paired with `--prefer`, enforced by
            // clap) reclaims the named workspace *with its working copy
            // intact* so a recovery re-dispatch can land back on the only
            // copy of a crashed worker's unpushed work. This deliberately
            // bypasses everything the normal path does to a dirty
            // workspace — it is NOT health-skipped, and the destructive
            // `jj git fetch && jj new main` reset is suppressed so the
            // uncommitted tree survives the lease.
            //
            // Unlike best-effort `--prefer`, this never silently falls
            // back to a fresh workspace: routing the recovering worker
            // away from the dirty tree is exactly the bug this flag
            // exists to fix (spinyfin/mono#963). If the named workspace
            // cannot be reclaimed as-is, fail loudly.
            if allow_dirty {
                let pref = prefer
                    .as_deref()
                    .expect("clap enforces --prefer when --allow-dirty is set");

                let target = store
                    .list_workspaces_filtered(&WorkspaceListFilter {
                        repo: Some(&repo),
                        workspace_id: Some(pref),
                        ..Default::default()
                    })?
                    .into_iter()
                    .next()
                    .ok_or_else(|| CubeError::WorkspaceNotFound(pref.to_string()))?;

                if target.state == WorkspaceState::Leased {
                    // Held by a live worker (or an unexpired lease). We must
                    // not stomp on it; the operator should force-release
                    // first if the holder is genuinely gone.
                    return Err(CubeError::InvalidArgument(format!(
                        "workspace `{pref}` is currently leased; cannot reclaim it dirty. \
                         Use `cube workspace force-release` first if the prior holder is gone."
                    )));
                }

                if !workspace_path_exists(&target) {
                    return Err(CubeError::WorkspaceNotFound(pref.to_string()));
                }
                // A directory with neither .jj/ nor .git/ holds no
                // recoverable work — there is nothing dirty to reclaim.
                if !target.workspace_path.join(".jj").is_dir() && !target.workspace_path.join(".git").is_dir() {
                    return Err(CubeError::InvalidArgument(format!(
                        "workspace `{pref}` has neither a .jj nor a .git directory; \
                         there is no in-flight work to reclaim with --allow-dirty."
                    )));
                }

                let mut workspace = store
                    .claim_specific_workspace(
                        &repo,
                        pref,
                        &holder,
                        &task,
                        &lease_id,
                        leased_at_epoch_s,
                        lease_expires_at,
                    )?
                    .ok_or_else(|| CubeError::NoAvailableWorkspace(repo.clone()))?;

                // The workspace is now claimed (state=leased) and exclusively
                // ours, so release the per-repo lock before the local jj probe
                // and setup below — none of that needs to serialize against
                // other leases, and holding the lock across it would let one
                // slow workspace wedge the whole repo pool.
                drop(_lock);

                // No reset: the working copy is handed over exactly as the
                // prior holder left it. Record whatever `@` currently is so
                // the registry's head_commit reflects the recovered state.
                let head_commit = current_workspace_commit(runner, database_path, &workspace.workspace_path)?;
                store.update_workspace_head_commit(&lease_id, Some(&head_commit))?;
                workspace.head_commit = Some(head_commit);

                // Positive recovery check. `--allow-dirty` says "hand me this
                // workspace without resetting it"; it does NOT establish that
                // there is still anything in it to recover. A workspace that
                // was already reset (by a release before the preservation
                // guard existed, by `--force-reset`, or by pool GC) is handed
                // over just as happily — and a caller that assumes recovery
                // succeeded because the lease succeeded will silently start
                // from an empty tree.
                //
                // So probe and report. `dirty_verified: false` is the signal
                // that cube could NOT recover the work and the caller must
                // fall back to its own artifact (the Boss engine's recovery
                // patch). No fetch here: an extra network round trip on the
                // recovery path is not worth it, and the verdict only turns on
                // the remote view when `@` is non-empty — in which case there
                // is demonstrably a working copy to hand back either way.
                let recovery_probe = read_head_status(
                    runner,
                    database_path,
                    &workspace.workspace_path,
                    &repo_record.main_branch,
                );
                let (dirty_verified, dirty_probe_error) = match &recovery_probe {
                    Ok(status) => (!status.is_reusable(), None),
                    Err(e) => {
                        eprintln!(
                            "warning: cube could not probe {} for recoverable state: {e}; \
                             reporting dirty_verified=false so the caller falls back rather than \
                             assuming a recovery that may not have happened",
                            workspace.workspace_id,
                        );
                        (false, Some(e.to_string()))
                    }
                };
                if !dirty_verified {
                    eprintln!(
                        "cube: reclaimed {} with --allow-dirty but its working copy holds no \
                         unrecovered work (dirty_verified=false); nothing was recovered in place.",
                        workspace.workspace_id,
                    );
                }

                audit!(
                    database_path,
                    "lease.acquired_dirty",
                    repo = workspace.repo,
                    workspace_id = workspace.workspace_id,
                    lease_id = lease_id,
                    holder = holder,
                    task = task,
                    head_commit = workspace.head_commit,
                    dirty_verified = dirty_verified,
                    dirty_head_change_id = recovery_probe.as_ref().ok().map(|s| s.head_change_id()),
                    dirty_unpushed_commits = recovery_probe.as_ref().ok().map(|s| s.unpushed_summary()),
                    dirty_probe_error = dirty_probe_error.as_deref(),
                );

                let setup_report = run_setup_for_workspace(&store, runner, &workspace)?;

                let lease_message = format!(
                    "Reclaimed {} (dirty) at {}.",
                    workspace.workspace_id,
                    workspace.workspace_path.display()
                );

                if let Some(failure) = setup_report.first_failure() {
                    let StepStatus::Failed { error } = &failure.status else {
                        unreachable!("first_failure returned non-failure step");
                    };
                    return Err(CubeError::SetupStepFailed {
                        step: failure.id.clone(),
                        error: error.clone(),
                    });
                }

                let message = format_lease_message(&lease_message, &setup_report);
                return RunResult::new(
                    message,
                    json!({
                        "workspace": workspace,
                        "setup": setup_report,
                        "health_check": [json!({
                            "workspace_id": pref,
                            "allow_dirty": true,
                            "reset_skipped": true,
                            // Whether the workspace still held unrecovered work
                            // at reclaim time. See the probe above: `false`
                            // means cube recovered nothing and the caller must
                            // fall back to its own recovery artifact.
                            "dirty_verified": dirty_verified,
                            "dirty_head_change_id": recovery_probe
                                .as_ref()
                                .ok()
                                .map(|s| s.head_change_id().to_string()),
                            "dirty_unpushed_commits": recovery_probe
                                .as_ref()
                                .ok()
                                .map(|s| s.unpushed_summary()),
                            "dirty_probe_error": dirty_probe_error,
                        })],
                    }),
                );
            }

            // ── Health-scan phase ───────────────────────────────────────────
            // Before claiming any workspace, inspect a *bounded* set of free
            // candidates:
            //   - Clean → use immediately (update DB if stale-dirty)
            //   - ConflictedBookmarks → save as first repairable candidate
            //     (keep looking for a clean one; repair before claim)
            //   - DirtyWorkingCopy → skip and mark in the store so
            //     `cube workspace list` surfaces it
            //
            // The repo lock is held throughout, so no concurrent lease can
            // steal a workspace between the health check and the claim.
            //
            // ## Why this is bounded, and what "bounded" means here
            //
            // The registry already records each free workspace's health in
            // SQLite (`health_status`, plus `unhealthy_since_epoch_s`). This
            // scan used to ignore that and re-derive it from the filesystem
            // for *every* free workspace on *every* lease — an O(free pool)
            // serial walk of `jj status` calls whose answer was already
            // committed. On a pool whose free workspaces had all gone dirty
            // that cost ~17s per lease and produced 18,374
            // `workspace.health_check_skipped reason=dirty_working_copy`
            // events against 259 leases: every single probe re-confirmed a
            // fact the DB held, and the lease provisioned a fresh workspace at
            // the end regardless.
            //
            // So: the cached verdict is authoritative, and the scan is bounded
            // three ways —
            //   1. Workspaces cached as unhealthy are NOT probed, except for a
            //      small rotating revalidation window (see
            //      [`STALE_HEALTH_REVALIDATE_PER_LEASE`]) that keeps a
            //      cleaned-out-of-band workspace from being withheld forever.
            //   2. At most [`LEASE_HEALTH_SCAN_MAX_PROBES`] live probes run.
            //   3. The whole scan stops at [`LEASE_HEALTH_SCAN_BUDGET`].
            // Hitting any bound is not a failure: provisioning is the designed
            // fallback and always succeeds for a reachable repo.
            //
            // Retention itself (why the cached-unhealthy set grows without
            // limit in the first place) is bounded separately by the pool GC's
            // aged-unhealthy reclaim, not here.
            let scan_started = Instant::now();
            let scan_deadline = scan_started + LEASE_HEALTH_SCAN_BUDGET;

            let effective_free = store.list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some(&repo),
                effective_state: Some(EffectiveState::Free),
                ..Default::default()
            })?;

            // Free workspaces whose cached health in the DB is dirty or
            // conflicted. These are NOT scanned wholesale any more; only the
            // rotating revalidation window below (and an explicit `--prefer`)
            // reaches them.
            let cached_unhealthy: Vec<WorkspaceRecord> = {
                let mut d = store.list_workspaces_filtered(&WorkspaceListFilter {
                    repo: Some(&repo),
                    effective_state: Some(EffectiveState::FreeDirty),
                    ..Default::default()
                })?;
                let mut c = store.list_workspaces_filtered(&WorkspaceListFilter {
                    repo: Some(&repo),
                    effective_state: Some(EffectiveState::FreeConflicted),
                    ..Default::default()
                })?;
                d.append(&mut c);
                d.sort_by(|a, b| a.workspace_id.cmp(&b.workspace_id));
                d
            };

            // Rotate the revalidation window across leases so every cached
            // -unhealthy workspace comes up for re-probe on a bounded
            // schedule. The cursor is persisted per repo; a failure to read or
            // write it degrades to "start from the top", never to a longer
            // scan.
            let cursor_key = stale_health_cursor_key(&repo);
            let cursor = store.get_pool_metadata_i(&cursor_key).unwrap_or(None).unwrap_or(0);
            let (revalidate_idx, next_cursor) =
                stale_revalidation_window(cached_unhealthy.len(), cursor, STALE_HEALTH_REVALIDATE_PER_LEASE);
            if !revalidate_idx.is_empty()
                && let Err(e) = store.set_pool_metadata_i(&cursor_key, next_cursor)
            {
                eprintln!("warning: cube could not persist the health revalidation cursor for `{repo}`: {e}");
            }
            let revalidate: Vec<&WorkspaceRecord> = revalidate_idx.iter().map(|i| &cached_unhealthy[*i]).collect();

            // All free candidates, used only so `--prefer` can name a
            // cached-unhealthy workspace outside the revalidation window.
            let all_free: Vec<&WorkspaceRecord> = effective_free.iter().chain(cached_unhealthy.iter()).collect();

            // Ordering: the --prefer workspace first (from either category —
            // an explicit request always earns its probe), then effective-free
            // candidates, then this lease's revalidation window.
            // Workspaces listed in --exclude are skipped entirely so the engine
            // can avoid re-offering a workspace it just refused (e.g. occupancy
            // guard) without looping forever on the same candidate.
            let ordered: Vec<&WorkspaceRecord> = {
                let mut queue: Vec<&WorkspaceRecord> = Vec::new();
                if let Some(pref) = prefer.as_deref()
                    && let Some(w) = all_free.iter().find(|w| w.workspace_id == pref)
                {
                    queue.push(w);
                }
                queue.extend(effective_free.iter());
                queue.extend(revalidate.iter().copied());

                let mut v: Vec<&WorkspaceRecord> = Vec::new();
                for w in queue {
                    if !v.iter().any(|e| e.workspace_id == w.workspace_id) && !exclude.contains(&w.workspace_id) {
                        v.push(w);
                    }
                }
                v
            };

            let mut health_checks: Vec<serde_json::Value> = Vec::new();
            // Live probes actually performed, and how the scan ended.
            let mut probes = 0usize;
            let mut revalidations = 0usize;
            let mut promoted_to_clean = 0usize;
            let mut scan_truncated: Option<&'static str> = None;
            // first clean workspace found
            let mut clean_candidate: Option<String> = None;
            // first conflicted-but-repairable workspace found
            let mut conflicted_candidate: Option<(String, Vec<String>)> = None;
            // Free workspaces whose directory exists but has neither .jj/ nor
            // .git/ — husks holding no recoverable work. Collected as
            // (workspace_id, path) so the lease path can GC them and reuse the
            // freed slot rather than surfacing a "broken-empty" failure.
            let mut broken_empty: Vec<(String, PathBuf)> = Vec::new();

            for ws in &ordered {
                let ws_id = &ws.workspace_id;
                if probes >= LEASE_HEALTH_SCAN_MAX_PROBES {
                    scan_truncated = Some("max_probes");
                    break;
                }
                if Instant::now() >= scan_deadline {
                    scan_truncated = Some("time_budget");
                    break;
                }

                if !workspace_path_exists(ws) {
                    // Will be reconciled; skip for health-check purposes.
                    health_checks.push(json!({
                        "workspace_id": ws_id,
                        "skipped": true,
                        "reason": "directory_missing",
                    }));
                    continue;
                }

                probes += 1;
                if matches!(
                    ws.health_status,
                    Some(WorkspaceHealth::Dirty) | Some(WorkspaceHealth::Conflicted)
                ) {
                    revalidations += 1;
                }

                // Any error from check_workspace_health skips this workspace
                // rather than hard-failing the lease. The pool is not fixed —
                // cube provisions a fresh workspace when none are usable.
                let outcome = match check_workspace_health(runner, database_path, &ws.workspace_path) {
                    Ok(o) => o,
                    Err(e) => {
                        let err_str = e.to_string();
                        health_checks.push(json!({
                            "workspace_id": ws_id,
                            "health": "error",
                            "skipped": true,
                            "reason": "health_check_error",
                            "error": err_str,
                        }));
                        audit!(
                            database_path,
                            "workspace.health_check_error",
                            repo = repo,
                            workspace_id = ws_id,
                            error = err_str,
                        );
                        continue;
                    }
                };
                match outcome {
                    WorkspaceHealthOutcome::Clean => {
                        let was_stale_dirty = matches!(
                            ws.health_status,
                            Some(WorkspaceHealth::Dirty) | Some(WorkspaceHealth::Conflicted)
                        );
                        health_checks.push(json!({
                            "workspace_id": ws_id,
                            "health": "clean",
                            "skipped": false,
                            "was_stale_dirty": was_stale_dirty,
                        }));
                        // If the workspace was previously marked unhealthy in the
                        // DB (stale dirty/conflicted), update the DB so
                        // subsequent `list` and GC passes see the correct state.
                        if was_stale_dirty {
                            promoted_to_clean += 1;
                            store.update_workspace_health(&repo, ws_id, WorkspaceHealth::Clean)?;
                            audit!(
                                database_path,
                                "workspace.health_reconciled",
                                repo = repo,
                                workspace_id = ws_id,
                                prior_health = ws.health_status.map(|h| h.as_str()).unwrap_or("unknown"),
                                new_health = "clean",
                            );
                        }
                        clean_candidate = Some(ws_id.clone());
                        break;
                    }
                    WorkspaceHealthOutcome::StaleWorkingCopy(recovery_kind) => {
                        // Run `jj workspace update-stale` to recover the working
                        // copy, then retry the health check. If recovery or the
                        // retry fails, skip this workspace; never hard-fail the
                        // lease over one bad workspace.
                        let update_stale_inv = CommandInvocation {
                            cwd: ws.workspace_path.clone(),
                            program: "jj".to_string(),
                            args: vec!["workspace".to_string(), "update-stale".to_string()],
                            env: vec![],
                        };
                        match runner.run(&update_stale_inv) {
                            Ok(_) => {
                                audit!(
                                    database_path,
                                    recovery_kind,
                                    workspace_path = ws.workspace_path.display().to_string(),
                                    repo = repo,
                                    workspace_id = ws_id,
                                );
                                match check_workspace_health(runner, database_path, &ws.workspace_path) {
                                    Ok(WorkspaceHealthOutcome::Clean) => {
                                        let was_stale_dirty = matches!(
                                            ws.health_status,
                                            Some(WorkspaceHealth::Dirty) | Some(WorkspaceHealth::Conflicted)
                                        );
                                        health_checks.push(json!({
                                            "workspace_id": ws_id,
                                            "health": "stale_recovered_clean",
                                            "skipped": false,
                                            "was_stale_dirty": was_stale_dirty,
                                        }));
                                        if was_stale_dirty {
                                            promoted_to_clean += 1;
                                            store.update_workspace_health(&repo, ws_id, WorkspaceHealth::Clean)?;
                                            audit!(
                                                database_path,
                                                "workspace.health_reconciled",
                                                repo = repo,
                                                workspace_id = ws_id,
                                                prior_health =
                                                    ws.health_status.map(|h| h.as_str()).unwrap_or("unknown"),
                                                new_health = "clean",
                                            );
                                        }
                                        clean_candidate = Some(ws_id.clone());
                                        break;
                                    }
                                    Ok(other) => {
                                        let reason = match &other {
                                            WorkspaceHealthOutcome::DirtyWorkingCopy => "dirty_after_stale_recovery",
                                            WorkspaceHealthOutcome::ConflictedBookmarks(_) => {
                                                "conflicted_after_stale_recovery"
                                            }
                                            WorkspaceHealthOutcome::StaleWorkingCopy(_) => "still_stale_after_recovery",
                                            WorkspaceHealthOutcome::BrokenEmpty => "broken_empty_after_stale_recovery",
                                            WorkspaceHealthOutcome::Clean => unreachable!(),
                                        };
                                        health_checks.push(json!({
                                            "workspace_id": ws_id,
                                            "health": "stale",
                                            "skipped": true,
                                            "reason": reason,
                                        }));
                                        audit!(
                                            database_path,
                                            "workspace.health_check_skipped",
                                            repo = repo,
                                            workspace_id = ws_id,
                                            reason = reason,
                                        );
                                    }
                                    Err(e) => {
                                        let err_str = e.to_string();
                                        health_checks.push(json!({
                                            "workspace_id": ws_id,
                                            "health": "stale",
                                            "skipped": true,
                                            "reason": "health_check_failed_after_stale_recovery",
                                            "error": err_str,
                                        }));
                                        audit!(
                                            database_path,
                                            "workspace.health_check_skipped",
                                            repo = repo,
                                            workspace_id = ws_id,
                                            reason = "health_check_failed_after_stale_recovery",
                                            error = err_str,
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                let err_str = e.to_string();
                                health_checks.push(json!({
                                    "workspace_id": ws_id,
                                    "health": "stale",
                                    "skipped": true,
                                    "reason": "stale_recovery_failed",
                                    "error": err_str,
                                }));
                                audit!(
                                    database_path,
                                    "workspace.health_check_skipped",
                                    repo = repo,
                                    workspace_id = ws_id,
                                    reason = "stale_recovery_failed",
                                    error = err_str,
                                );
                            }
                        }
                    }
                    WorkspaceHealthOutcome::ConflictedBookmarks(ref bookmarks) => {
                        health_checks.push(json!({
                            "workspace_id": ws_id,
                            "health": "conflicted",
                            "bookmarks": bookmarks,
                            "skipped": conflicted_candidate.is_some(),
                        }));
                        store.update_workspace_health(&repo, ws_id, WorkspaceHealth::Conflicted)?;
                        if conflicted_candidate.is_none() {
                            conflicted_candidate = Some((ws_id.clone(), bookmarks.clone()));
                        }
                        // Keep looking for a clean one before falling back
                        // to repairing the conflicted one.
                    }
                    WorkspaceHealthOutcome::DirtyWorkingCopy => {
                        health_checks.push(json!({
                            "workspace_id": ws_id,
                            "health": "dirty",
                            "skipped": true,
                            "reason": "dirty_working_copy",
                        }));
                        store.update_workspace_health(&repo, ws_id, WorkspaceHealth::Dirty)?;
                        audit!(
                            database_path,
                            "workspace.health_check_skipped",
                            repo = repo,
                            workspace_id = ws_id,
                            reason = "dirty_working_copy",
                        );
                    }
                    WorkspaceHealthOutcome::BrokenEmpty => {
                        let ws_path_str = ws.workspace_path.display().to_string();
                        health_checks.push(json!({
                            "workspace_id": ws_id,
                            "workspace_path": ws_path_str,
                            "health": "broken_empty",
                            "has_git": false,
                            "has_jj": false,
                            "skipped": true,
                            "reason": "neither_git_nor_jj",
                        }));
                        broken_empty.push((ws_id.clone(), ws.workspace_path.clone()));
                        audit!(
                            database_path,
                            "workspace.broken_empty",
                            repo = repo,
                            workspace_id = ws_id,
                            workspace_path = ws_path_str,
                        );
                    }
                }
            }

            // One aggregate event for the whole scan, instead of one per
            // candidate. The per-candidate `workspace.health_check_skipped`
            // events above still fire for workspaces this lease actually
            // probed — they are now bounded to LEASE_HEALTH_SCAN_MAX_PROBES
            // per lease rather than one per free workspace, which is what
            // turned the audit log into 18k rows of the same fact.
            let scan_elapsed_ms = scan_started.elapsed().as_millis() as u64;
            let scan_outcome = if clean_candidate.is_some() {
                "clean"
            } else if conflicted_candidate.is_some() {
                "conflicted_repairable"
            } else {
                "none"
            };
            audit!(
                database_path,
                "workspace.health_scan",
                repo = repo,
                effective_free = effective_free.len(),
                cached_unhealthy = cached_unhealthy.len(),
                trusted_cache_skips = cached_unhealthy.len().saturating_sub(revalidations),
                probed = probes,
                revalidated = revalidations,
                promoted_to_clean = promoted_to_clean,
                truncated = scan_truncated,
                elapsed_ms = scan_elapsed_ms,
                outcome = scan_outcome,
            );
            let scan_summary = json!({
                "effective_free": effective_free.len(),
                "cached_unhealthy": cached_unhealthy.len(),
                "trusted_cache_skips": cached_unhealthy.len().saturating_sub(revalidations),
                "probed": probes,
                "max_probes": LEASE_HEALTH_SCAN_MAX_PROBES,
                "revalidated": revalidations,
                "promoted_to_clean": promoted_to_clean,
                "truncated": scan_truncated,
                "elapsed_ms": scan_elapsed_ms,
                "outcome": scan_outcome,
            });

            // Decide which workspace to use: prefer clean, fall back to the
            // first repairable conflicted workspace, then to a quarantined one
            // that can be verifiably reclaimed, otherwise auto-create a fresh
            // one. No pool state (dirty, husk, occupied) is ever a hard stop —
            // cube always provisions new capacity for a reachable repo.
            let mut chosen_id = clean_candidate.or_else(|| conflicted_candidate.as_ref().map(|(id, _)| id.clone()));

            // Growing the pool is the last resort, not the first: a quarantined
            // workspace nobody is working in is perfectly good capacity, and
            // minting instead of reclaiming it is how the pool grew to hundreds
            // of permanently ineligible entries. The reclaim is verified (see
            // `reclaim_quarantined_workspace`), so one whose `@` still holds an
            // unpushed working copy stays quarantined and we do mint.
            if chosen_id.is_none() {
                match reclaim_quarantined_workspace(
                    runner,
                    &store,
                    database_path,
                    &repo,
                    &repo_record.main_branch,
                    &exclude,
                    Some(Instant::now() + QUARANTINE_RECLAIM_BUDGET),
                ) {
                    Ok(Some(reclaimed_id)) => {
                        health_checks.push(json!({
                            "workspace_id": reclaimed_id,
                            "health": "quarantine_reclaimed",
                            "skipped": false,
                        }));
                        chosen_id = Some(reclaimed_id);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("cube: quarantine reclaim: scan failed, provisioning instead: {e}");
                    }
                }
            }

            let (mut workspace, mut was_auto_created, repair_bookmarks) = if let Some(ws_id) = chosen_id {
                // Claim the specific workspace we health-checked.
                let ws = store
                    .claim_specific_workspace(
                        &repo,
                        &ws_id,
                        &holder,
                        &task,
                        &lease_id,
                        leased_at_epoch_s,
                        lease_expires_at,
                    )?
                    .ok_or_else(|| CubeError::NoAvailableWorkspace(repo.clone()))?;
                let bookmarks = conflicted_candidate
                    .filter(|(id, _)| *id == ws_id)
                    .map(|(_, b)| b)
                    .unwrap_or_default();
                (ws, false, bookmarks)
            } else {
                // No clean or repairable workspace. Self-heal any broken-empty
                // husks first: a directory with neither .jj/ nor .git/ holds no
                // recoverable work, so delete it and free its slot instead of
                // surfacing it to the caller. A husk must never be a reason to
                // deny a lease for a reachable repo (issue #845 part 2b).
                for (ws_id, ws_path) in &broken_empty {
                    gc_broken_empty_workspace(&mut store, database_path, &repo, ws_id, ws_path)?;
                    candidates.retain(|c| &c.workspace_id != ws_id);
                }

                // Pool is empty, fully leased, only had broken-empty husks
                // (now GC'd), or all free slots are dirty: grow the pool by
                // one. Dirty workspaces are left untouched so their unpushed
                // work is preserved for later inspection. The pool is an
                // optimisation (reuse a known-good checkout), never a hard
                // cap — a lease for a reachable repo always succeeds.
                let new_candidate = auto_create_workspace(runner, &repo_record, &candidates)?;
                let new_id = new_candidate.workspace_id.clone();
                candidates.push(new_candidate);
                store.sync_workspaces(&repo, &candidates)?;
                // Claim the workspace we just created by id. A generic claim
                // could otherwise grab a leftover free-but-dirty workspace and
                // destructively reset it — the dirty entries are intentionally
                // preserved for the operator.
                let ws = store
                    .claim_specific_workspace(
                        &repo,
                        &new_id,
                        &holder,
                        &task,
                        &lease_id,
                        leased_at_epoch_s,
                        lease_expires_at,
                    )?
                    .ok_or_else(|| CubeError::NoAvailableWorkspace(repo.clone()))?;
                (ws, true, vec![])
            };

            if !workspace_path_exists(&workspace) {
                // Reconcile above should have caught this on the pre-claim
                // pass, but a concurrent `rm -rf` between reconcile and
                // claim can still land here. Drop the row and surface a
                // warning so the operator sees what happened.
                eprintln!(
                    "warning: cube workspace `{}/{}` directory disappeared between reconcile \
                     and claim at {}; dropping the dangling registry row",
                    workspace.repo,
                    workspace.workspace_id,
                    workspace.workspace_path.display(),
                );
                audit!(
                    database_path,
                    "workspace.dir_missing_reconciled",
                    repo = workspace.repo,
                    workspace_id = workspace.workspace_id,
                    workspace_path = workspace.workspace_path.display().to_string(),
                    prior_state = workspace.state.as_str(),
                    lease_id = lease_id,
                );
                store.forget_workspace(&workspace.repo, &workspace.workspace_id)?;
                return Err(CubeError::NoAvailableWorkspace(repo));
            }

            // ── Critical section ends here ──────────────────────────────────
            // The workspace is claimed (state=leased) and exclusively ours.
            // Release the per-repo lock before the network-bound reset/resume
            // and setup below: those operate only on this one workspace, and
            // the bounded lease TTL (30m) far exceeds the bounded reset, so no
            // concurrent lease can expire-and-reclaim it mid-reset. Holding
            // the lock across `jj git fetch` is exactly what let one stalled
            // workspace wedge every other lease/release for the repo.
            drop(_lock);

            // If the workspace had conflicted bookmarks, repair them before
            // the reset. `jj new main` would succeed with conflicts present,
            // but the conflicts would still appear in `jj status` for the
            // new worker — better to clean them up now so the workspace is
            // truly pristine.
            if !repair_bookmarks.is_empty()
                && let Err(error) =
                    repair_conflicted_bookmarks(runner, database_path, &workspace.workspace_path, &repair_bookmarks)
            {
                let _ = store.release_workspace(&lease_id, Some("lease_setup_failed"));
                return Err(error);
            }

            // If the workspace we just claimed was reclaimed-from-expired
            // in this lease call, guard the reset: a destructive
            // `jj new <main>` against a workspace whose prior lease
            // holder is still active would silently destroy their
            // working copy. This is exactly the race Worf reported on
            // 2026-05-12 ("`@` got re-pointed at unrelated commits").
            //
            // Auto-created workspaces just came out of `jj git clone`,
            // so there is no prior worker's `@` to protect — skip the
            // guard in that case to avoid spurious refusals after the
            // reconcile-and-replace path discards a dangling row.
            let prior_expired = if was_auto_created {
                None
            } else {
                expired_by_workspace_id.get(workspace.workspace_id.as_str()).copied()
            };
            if let Err(error) = reset_workspace_guarded(
                runner,
                database_path,
                &workspace.workspace_path,
                &repo_record.main_branch,
                prior_expired,
            ) {
                let CubeError::LeaseExpiredWorkspaceDirty {
                    prior_lease_id: refused_prior_lease_id,
                    prior_holder: refused_prior_holder,
                    ..
                } = error
                else {
                    let _ = store.release_workspace(&lease_id, Some("lease_setup_failed"));
                    return Err(error);
                };

                // The health check a few hundred lines up only tests for
                // uncommitted *file* changes (`jj status`'s "Working copy
                // changes:" section); the guard above tests a stricter
                // condition (`@` empty AND its parent is `main`). A
                // workspace whose prior expired-lease holder ran `jj
                // describe` (committing their WIP) but never pushed can
                // pass the health check as Clean yet still fail the guard
                // above — so a workspace can pass selection and still hit
                // this refusal. No pool state is ever allowed to hard-fail
                // a lease call for a reachable repo (see "No pool state
                // ... is ever a hard stop" above), so: durably quarantine
                // this workspace — unlike `prior_expired` above (which only
                // guards this one lease call), the quarantine health status
                // persists across the release below and blocks every later
                // lease call from selecting it — and fall back to a
                // freshly auto-created workspace, which can never trip
                // this guard (a brand new `jj workspace add` has no prior
                // holder's work to protect, so `prior_expired` is always
                // `None` for it below — this cannot recurse).
                if let Err(store_err) = store.quarantine_workspace(&lease_id, DIRTY_RECLAIM_QUARANTINE_REASON) {
                    eprintln!(
                        "warning: cube failed to persist dirty-reclaim quarantine for `{}/{}`: {store_err}",
                        workspace.repo, workspace.workspace_id,
                    );
                }
                audit!(
                    database_path,
                    "workspace.dirty_reclaim_quarantined",
                    repo = workspace.repo,
                    workspace_id = workspace.workspace_id,
                    prior_lease_id = refused_prior_lease_id,
                    prior_holder = refused_prior_holder,
                );

                let retry_lock = RepoLock::acquire(&repo_lock_path(&repo, database_path)?)?;
                let mut retry_candidates = discover_workspaces(&repo_record)?;
                store.sync_workspaces(&repo, &retry_candidates)?;
                let new_candidate = auto_create_workspace(runner, &repo_record, &retry_candidates)?;
                let new_id = new_candidate.workspace_id.clone();
                retry_candidates.push(new_candidate);
                store.sync_workspaces(&repo, &retry_candidates)?;
                workspace = store
                    .claim_specific_workspace(
                        &repo,
                        &new_id,
                        &holder,
                        &task,
                        &lease_id,
                        leased_at_epoch_s,
                        lease_expires_at,
                    )?
                    .ok_or_else(|| CubeError::NoAvailableWorkspace(repo.clone()))?;
                was_auto_created = true;
                drop(retry_lock);

                if !workspace_path_exists(&workspace) {
                    eprintln!(
                        "warning: cube workspace `{}/{}` directory disappeared between provisioning \
                         and claim at {}; dropping the dangling registry row",
                        workspace.repo,
                        workspace.workspace_id,
                        workspace.workspace_path.display(),
                    );
                    store.forget_workspace(&workspace.repo, &workspace.workspace_id)?;
                    return Err(CubeError::NoAvailableWorkspace(repo));
                }

                if let Err(error) = reset_workspace_guarded(
                    runner,
                    database_path,
                    &workspace.workspace_path,
                    &repo_record.main_branch,
                    None,
                ) {
                    let _ = store.release_workspace(&lease_id, Some("lease_setup_failed"));
                    return Err(error);
                }
            }

            let head_commit = current_workspace_commit(runner, database_path, &workspace.workspace_path)?;
            store.update_workspace_head_commit(&lease_id, Some(&head_commit))?;
            workspace.head_commit = Some(head_commit);

            audit!(
                database_path,
                "lease.acquired",
                repo = workspace.repo,
                workspace_id = workspace.workspace_id,
                lease_id = lease_id,
                holder = holder,
                task = task,
                head_commit = workspace.head_commit,
                was_auto_created = was_auto_created,
            );

            // Defense-in-depth (issue #1174): keep Boss/host infra files —
            // an empty `logs/<workspace>.log`, the engine's `.boss/` scratch
            // dir — out of the worker's jj snapshot so they never get
            // committed into a PR. Done before setup runs, so a setup step
            // that drops such a file is already covered.
            ensure_boss_infra_excluded(&workspace.workspace_path, &workspace.workspace_id);

            let setup_report = run_setup_for_workspace(&store, runner, &workspace)?;

            let lease_message = format!(
                "Leased {} at {}.",
                workspace.workspace_id,
                workspace.workspace_path.display()
            );

            if let Some(failure) = setup_report.first_failure() {
                let StepStatus::Failed { error } = &failure.status else {
                    unreachable!("first_failure returned non-failure step");
                };
                if release_on_setup_failure {
                    // Fail-closed for the engine (the anaplian failure-mode A
                    // leak): a setup failure the caller can't repair must not
                    // leave a leased-but-unusable workspace stranded — cube
                    // granted the lease, a setup step exited non-zero, and the
                    // caller never learned the lease id to release it. Hand the
                    // workspace straight back (force-release skips the reset;
                    // the next lease resets and re-runs setup), then surface the
                    // original setup error with nothing left leased.
                    match store.force_release_lease(&lease_id, Some("setup_failed")) {
                        Ok(_) => audit!(
                            database_path,
                            "lease.released_on_setup_failure",
                            repo = workspace.repo,
                            workspace_id = workspace.workspace_id,
                            lease_id = lease_id,
                            failed_step = failure.id,
                        ),
                        Err(release_err) => {
                            // Best-effort: still surface the original setup
                            // failure even if the compensating release fails,
                            // but make the release failure visible.
                            eprintln!(
                                "warning: failed to release lease {lease_id} after setup step `{}` failed: {release_err}",
                                failure.id,
                            );
                        }
                    }
                }
                // Default (interactive) behavior: the lease is retained so the
                // workspace can be repaired via `cube workspace setup` or
                // released explicitly. The error surfaces the failed step so
                // callers can decide how to recover.
                return Err(CubeError::SetupStepFailed {
                    step: failure.id.clone(),
                    error: error.clone(),
                });
            }

            let message = format_lease_message(&lease_message, &setup_report);
            let payload = json!({
                "workspace": workspace,
                "setup": setup_report,
                "health_check": health_checks,
                "health_scan": scan_summary,
            });
            RunResult::new(message, payload)
        }
        WorkspaceCommand::Release {
            workspace,
            lease,
            repo,
            reason,
            keep_dirty,
            force_reset,
        } => {
            let lease = resolve_release_lease(&mut store, workspace, lease, repo)?;
            let workspace = store
                .get_workspace_by_lease(&lease)?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;

            // Missing-dir handling mutates the registry, so it needs the lock;
            // take it only for that and return.
            if !workspace_path_exists(&workspace) {
                let _lock = RepoLock::acquire(&repo_lock_path(&workspace.repo, database_path)?)?;
                eprintln!(
                    "warning: cube workspace `{}/{}` directory is missing at {}; \
                     removing the dangling registry row instead of running release reset",
                    workspace.repo,
                    workspace.workspace_id,
                    workspace.workspace_path.display(),
                );
                audit!(
                    database_path,
                    "workspace.dir_missing_reconciled",
                    repo = workspace.repo,
                    workspace_id = workspace.workspace_id,
                    workspace_path = workspace.workspace_path.display().to_string(),
                    prior_state = workspace.state.as_str(),
                    lease_id = lease,
                );
                store.forget_workspace(&workspace.repo, &workspace.workspace_id)?;
                return Err(CubeError::LeaseNotFound(lease));
            }

            // Reset the workspace OUTSIDE the per-repo lock. This is the
            // root-cause fix: the workspace is still `leased` (so no concurrent
            // lease can claim it) and its TTL (30m) far exceeds the now-bounded
            // reset, so running `jj git fetch && jj new <main>` here cannot be
            // raced — and a stalled remote can no longer hold the lock and
            // wedge every other lease/release for the repo. A failed or
            // timed-out reset degrades to "release the lease anyway, mark the
            // workspace dirty" instead of blocking the release.
            let mut reset_error: Option<CubeError> = None;
            // Set when the release-time reuse guard refused the destructive
            // reset because `@` still holds work that exists on no remote.
            let mut preserved: Option<PreservedWorkingCopy> = None;
            if !keep_dirty {
                let repo_record = store
                    .get_repo(&workspace.repo)?
                    .ok_or_else(|| CubeError::RepoNotFound(workspace.repo.clone()))?;
                // Refresh the lease expiry before the unlocked reset so a
                // concurrent `cube workspace lease` cannot expire-and-reclaim
                // this still-`leased` workspace while we are mid-reset (the
                // window that opened up once the reset stopped holding the repo
                // lock). Best-effort: if the lease is already gone the reset
                // and release below surface it as LeaseNotFound as before.
                let _ = store.heartbeat_lease(&lease, Some(current_epoch_s()? + DEFAULT_LEASE_TTL_SECS));
                match reset_workspace_on_release(
                    runner,
                    database_path,
                    &workspace.workspace_path,
                    &repo_record.main_branch,
                    force_reset,
                ) {
                    Ok(ReleaseResetOutcome::Reset) => {
                        // Opportunistically forget consumed boss/exec_* bookmarks.
                        // The fetch above already updated main, so do_fetch = false.
                        // Best-effort: log a warning but never block the release.
                        match gc_workspace_bookmarks(runner, database_path, &workspace.workspace_path, false, false) {
                            Ok(forgotten) if !forgotten.is_empty() => {
                                eprintln!(
                                    "cube: release gc: {} consumed bookmark(s) forgotten in {}",
                                    forgotten.len(),
                                    workspace.workspace_id,
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!(
                                    "warning: bookmark gc on release of {} failed: {e}",
                                    workspace.workspace_id,
                                );
                            }
                        }
                    }
                    Ok(ReleaseResetOutcome::PreservedUnpushedWork(kept)) => {
                        eprintln!(
                            "cube: release of {} preserved the working copy: `@` ({}) holds work that \
                             exists on no remote. The workspace is freed but left dirty so the work \
                             stays reachable; re-lease it with `--prefer {} --allow-dirty` to resume, \
                             or release with `--force-reset` once you know the tree is disposable.",
                            workspace.workspace_id, kept.head_change_id, workspace.workspace_id,
                        );
                        preserved = Some(kept);
                    }
                    Err(e) => {
                        eprintln!(
                            "warning: workspace reset on release of {} failed: {e}; releasing the \
                             lease anyway and marking the workspace dirty so the next lease \
                             re-resets it before handing it out",
                            workspace.workspace_id,
                        );
                        reset_error = Some(e);
                    }
                }
            }

            // Take the lock only for the registry state transition.
            let _lock = RepoLock::acquire(&repo_lock_path(&workspace.repo, database_path)?)?;
            // A preserved working copy is the reason the workspace is being
            // freed dirty; record it in `last_release_reason` (overriding the
            // caller's annotation only when the caller gave none) so
            // `cube workspace list` explains the dirty row.
            let effective_reason = reason.clone().or_else(|| {
                preserved
                    .as_ref()
                    .map(|_| PRESERVED_UNPUSHED_RELEASE_REASON.to_string())
            });
            let released = store
                .release_workspace(&lease, effective_reason.as_deref())?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;
            if reset_error.is_some() || preserved.is_some() {
                // Either the freed workspace is in an unknown post-reset state,
                // or the guard deliberately left the prior holder's work in the
                // tree. Both must be flagged dirty (release_workspace cleared
                // health to NULL) so the lease health-check skips the workspace
                // rather than handing out an un-reset tree — and, for the
                // preserved case, so a fresh worker never lands on top of work
                // that is only in that tree.
                let _ = store.update_workspace_health(&released.repo, &released.workspace_id, WorkspaceHealth::Dirty);
            }
            drop(_lock);

            audit!(
                database_path,
                "lease.released",
                repo = released.repo,
                workspace_id = released.workspace_id,
                lease_id = lease,
                reason = effective_reason,
                keep_dirty = keep_dirty,
                force_reset = force_reset,
                preserved_unpushed = preserved.is_some(),
                preserved_head_change_id = preserved.as_ref().map(|p| p.head_change_id.as_str()),
                preserved_unpushed_commits = preserved.as_ref().map(|p| p.unpushed_summary.as_str()),
                reset_failed = reset_error.is_some(),
                reset_error = reset_error.as_ref().map(|e| e.to_string()),
            );

            let message = if keep_dirty {
                format!("Released {} (kept dirty).", released.workspace_id)
            } else if preserved.is_some() {
                format!(
                    "Released {} (working copy preserved: unpushed work).",
                    released.workspace_id
                )
            } else if reset_error.is_some() {
                format!("Released {} (reset failed; marked dirty).", released.workspace_id)
            } else {
                format!("Released {}.", released.workspace_id)
            };
            RunResult::new(
                message,
                json!({
                    "workspace": released,
                    "reset_failed": reset_error.is_some(),
                    "preserved_unpushed_work": preserved.is_some(),
                    "preserved_head_change_id": preserved.as_ref().map(|p| p.head_change_id.clone()),
                    "preserved_unpushed_commits": preserved.as_ref().map(|p| p.unpushed_summary.clone()),
                }),
            )
        }
        WorkspaceCommand::Heartbeat { lease, ttl_seconds } => {
            let now = current_epoch_s()?;
            let ttl = ttl_seconds.map(|s| s as i64).unwrap_or(DEFAULT_LEASE_TTL_SECS);
            let new_expires_at = now + ttl;
            let updated = store
                .heartbeat_lease(&lease, Some(new_expires_at))?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;
            RunResult::new(
                format!(
                    "Heartbeat lease {}; new expiry {} (in {}s).",
                    lease, new_expires_at, ttl
                ),
                json!({
                    "workspace": updated,
                }),
            )
        }
        WorkspaceCommand::ForceRelease {
            workspace,
            lease,
            repo,
            reason,
        } => {
            // Resolve the target row directly (not via `resolve_release_lease`,
            // which requires an active lease): a workspace the dirty-reclaim
            // guard quarantined is already `free` by the time an operator
            // salvages it with this command, so "not currently leased" must
            // not be treated as an error here.
            let target = if let Some(ref lease_id) = lease {
                store
                    .get_workspace_by_lease(lease_id)?
                    .ok_or_else(|| CubeError::LeaseNotFound(lease_id.clone()))?
            } else {
                let workspace_id = workspace.clone().ok_or_else(|| {
                    CubeError::InvalidArgument("release requires a workspace id positional or --lease".to_string())
                })?;
                let matches = store.list_workspaces_filtered(&WorkspaceListFilter {
                    repo: repo.as_deref(),
                    workspace_id: Some(&workspace_id),
                    ..Default::default()
                })?;
                match matches.as_slice() {
                    [] => return Err(CubeError::WorkspaceNotFound(workspace_id)),
                    [single] => single.clone(),
                    many => {
                        let repos = many.iter().map(|r| r.repo.as_str()).collect::<Vec<_>>().join(", ");
                        return Err(CubeError::InvalidArgument(format!(
                            "workspace id `{workspace_id}` matches multiple repos ({repos}); disambiguate with --repo"
                        )));
                    }
                }
            };

            // Repo-scoped lock so a concurrent normal release/lease can't race.
            let _lock = RepoLock::acquire(&repo_lock_path(&target.repo, database_path)?)?;

            if target.state == WorkspaceState::Leased {
                let lease_id = target.lease_id.clone().ok_or_else(|| {
                    CubeError::InvalidArgument(format!(
                        "workspace `{}/{}` is leased but has no lease id recorded",
                        target.repo, target.workspace_id
                    ))
                })?;
                let reason = reason.unwrap_or_else(|| "force-released".to_string());
                let released = store
                    .force_release_lease(&lease_id, Some(&reason))?
                    .ok_or_else(|| CubeError::LeaseNotFound(lease_id.clone()))?;

                audit!(
                    database_path,
                    "lease.force_released",
                    repo = released.repo,
                    workspace_id = released.workspace_id,
                    lease_id = lease_id,
                    reason = reason,
                );

                RunResult::new(
                    format!("Force-released {} (workspace not reset).", released.workspace_id),
                    json!({
                        "workspace": released,
                    }),
                )
            } else if target.health_status == Some(WorkspaceHealth::Quarantined) {
                // Salvage path for a `free-quarantined` row: clear the
                // durable marker the dirty-reclaim guard set (see
                // `Store::quarantine_workspace`) so the workspace re-enters
                // normal lease candidate discovery. Does not touch the
                // working copy — an operator should inspect it first.
                if !store.clear_workspace_quarantine(&target.repo, &target.workspace_id)? {
                    return Err(CubeError::InvalidArgument(format!(
                        "workspace `{}/{}` is no longer quarantined; nothing to clear",
                        target.repo, target.workspace_id
                    )));
                }

                audit!(
                    database_path,
                    "workspace.quarantine_cleared",
                    repo = target.repo,
                    workspace_id = target.workspace_id,
                );

                let cleared = store
                    .list_workspaces_filtered(&WorkspaceListFilter {
                        repo: Some(&target.repo),
                        workspace_id: Some(&target.workspace_id),
                        ..Default::default()
                    })?
                    .into_iter()
                    .next()
                    .ok_or_else(|| CubeError::WorkspaceNotFound(target.workspace_id.clone()))?;

                RunResult::new(
                    format!(
                        "Cleared dirty-reclaim quarantine on {} (workspace not reset; free for leasing again).",
                        cleared.workspace_id
                    ),
                    json!({
                        "workspace": cleared,
                    }),
                )
            } else {
                Err(CubeError::InvalidArgument(format!(
                    "workspace `{}/{}` is not leased and not quarantined ({}); nothing to force-release",
                    target.repo,
                    target.workspace_id,
                    effective_state_display(&target)
                )))
            }
        }
        WorkspaceCommand::Status { workspace } => {
            let path = PathBuf::from(&workspace);
            let record = find_workspace_record(&mut store, &path)?
                .ok_or_else(|| CubeError::WorkspaceNotFound(workspace.clone()))?;
            let jj_status = run_jj(
                runner,
                database_path,
                &RealCommandRunner::invocation(&path, "jj", &["status"]),
            )?;

            RunResult::new(
                human_workspace_detail(&record, &jj_status),
                json!({
                    "workspace": record,
                    "jj_status": jj_status,
                }),
            )
        }
        WorkspaceCommand::Setup { workspace } => {
            let path = PathBuf::from(&workspace);
            let record = find_workspace_record(&mut store, &path)?
                .ok_or_else(|| CubeError::WorkspaceNotFound(workspace.clone()))?;
            let report = run_setup_for_workspace(&store, runner, &record)?;
            let payload = json!({
                "workspace": record,
                "setup": report,
            });
            if let Some(failure) = report.first_failure() {
                let StepStatus::Failed { error } = &failure.status else {
                    unreachable!("first_failure returned non-failure step");
                };
                return Err(CubeError::SetupStepFailed {
                    step: failure.id.clone(),
                    error: error.clone(),
                });
            }
            let message = format_setup_message(&record.workspace_id, &report);
            RunResult::new(message, payload)
        }
        WorkspaceCommand::List { repo, state, holder } => {
            let parsed_effective_state = match state.as_deref() {
                Some(raw) => Some(raw.parse::<EffectiveState>().map_err(|()| {
                    CubeError::InvalidArgument(format!(
                        "invalid --state `{raw}`; expected `free`, `free-dirty`, `free-conflicted`, \
                         `free-quarantined`, or `leased`"
                    ))
                })?),
                None => None,
            };
            // Reconcile rows whose on-disk directory has been wiped before
            // we materialize the listing — otherwise `list` would surface
            // a row that the next `lease` is going to fail on. Scope the
            // reconcile to the same repo filter the user asked for.
            let reconciled =
                reconcile_missing_workspaces(&mut store, database_path, repo.as_deref(), current_epoch_s()?)?;
            let filter = WorkspaceListFilter {
                repo: repo.as_deref(),
                effective_state: parsed_effective_state,
                holder_pattern: holder.as_deref(),
                ..Default::default()
            };
            let records = store.list_workspaces_filtered(&filter)?;
            let message = format_workspace_list(&records);
            RunResult::new(
                message,
                json!({
                    "workspaces": records,
                    "reconciled": reconciled,
                }),
            )
        }
        WorkspaceCommand::Remove {
            workspace,
            repo,
            force,
            expunge,
        } => {
            let matches = store.list_workspaces_filtered(&WorkspaceListFilter {
                repo: repo.as_deref(),
                workspace_id: Some(&workspace),
                ..Default::default()
            })?;
            let record = match matches.as_slice() {
                [] => return Err(CubeError::WorkspaceNotFound(workspace)),
                [single] => single.clone(),
                many => {
                    let repos = many.iter().map(|r| r.repo.as_str()).collect::<Vec<_>>().join(", ");
                    return Err(CubeError::InvalidArgument(format!(
                        "workspace id `{workspace}` matches multiple repos ({repos}); disambiguate with --repo"
                    )));
                }
            };

            let _lock = RepoLock::acquire(&repo_lock_path(&record.repo, database_path)?)?;

            if record.state == WorkspaceState::Leased && !force {
                return Err(CubeError::InvalidArgument(format!(
                    "workspace `{}/{}` is currently leased; force-release it first or pass --force",
                    record.repo, record.workspace_id
                )));
            }

            store.forget_workspace(&record.repo, &record.workspace_id)?;

            if expunge {
                match fs::remove_dir_all(&record.workspace_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(CubeError::WorkspaceDirRemove {
                            path: record.workspace_path.clone(),
                            source: err,
                        });
                    }
                }
            }

            if let Err(e) = cleanup_workspace_logs(&record.workspace_id) {
                eprintln!(
                    "warning: failed to clean up workspace logs for {}: {e}",
                    record.workspace_id
                );
            }

            audit!(
                database_path,
                "workspace.removed",
                repo = record.repo,
                workspace_id = record.workspace_id,
                prior_state = record.state.as_str(),
                lease_id = record.lease_id,
                holder = record.holder,
                forced = force,
                expunged = expunge,
            );

            let message = if expunge {
                format!(
                    "Removed {}/{} from the registry and deleted workspace directory at {}.",
                    record.repo,
                    record.workspace_id,
                    record.workspace_path.display(),
                )
            } else {
                format!(
                    "Removed {}/{} from the registry (workspace directory left intact at {}).",
                    record.repo,
                    record.workspace_id,
                    record.workspace_path.display(),
                )
            };
            RunResult::new(
                message,
                json!({
                    "workspace": record,
                    "forced": force,
                    "expunged": expunge,
                }),
            )
        }
        WorkspaceCommand::Reconcile {
            repo,
            workspace,
            dry_run,
        } => {
            let report = reconcile_free_workspace_health(
                runner,
                &store,
                database_path,
                repo.as_deref(),
                workspace.as_deref(),
                dry_run,
                None,
            );

            let promoted = report.promoted_to_clean.len();
            let still = report.still_unhealthy.len();
            let skipped = report.skipped.len();

            let message = if dry_run {
                format!(
                    "Dry run: {} workspace(s) would be promoted to clean, {} still unhealthy, {} skipped.",
                    promoted, still, skipped,
                )
            } else if promoted > 0 {
                format!(
                    "Promoted {} workspace(s) to clean, {} still unhealthy, {} skipped.",
                    promoted, still, skipped,
                )
            } else {
                format!(
                    "No workspaces promoted. {} still unhealthy, {} skipped.",
                    still, skipped,
                )
            };

            RunResult::new(
                message,
                json!({
                    "dry_run": dry_run,
                    "promoted_to_clean": report.promoted_to_clean,
                    "still_unhealthy": report.still_unhealthy,
                    "skipped": report.skipped,
                }),
            )
        }
        WorkspaceCommand::Gc { workspace, dry_run } => {
            let records = store.list_workspaces_filtered(&WorkspaceListFilter {
                workspace_id: workspace.as_deref(),
                ..Default::default()
            })?;

            #[derive(serde::Serialize)]
            struct WorkspaceGcResult {
                workspace_id: String,
                bookmarks_forgotten: Vec<String>,
                skipped: bool,
                skipped_reason: Option<String>,
                error: Option<String>,
            }

            let mut results: Vec<WorkspaceGcResult> = Vec::new();
            for record in &records {
                if record.state == WorkspaceState::Leased {
                    results.push(WorkspaceGcResult {
                        workspace_id: record.workspace_id.clone(),
                        bookmarks_forgotten: vec![],
                        skipped: true,
                        skipped_reason: Some("leased".to_string()),
                        error: None,
                    });
                    continue;
                }
                if !workspace_path_exists(record) {
                    results.push(WorkspaceGcResult {
                        workspace_id: record.workspace_id.clone(),
                        bookmarks_forgotten: vec![],
                        skipped: true,
                        skipped_reason: Some("directory_missing".to_string()),
                        error: None,
                    });
                    continue;
                }
                match gc_workspace_bookmarks(runner, database_path, &record.workspace_path, true, dry_run) {
                    Ok(bookmarks) => {
                        results.push(WorkspaceGcResult {
                            workspace_id: record.workspace_id.clone(),
                            bookmarks_forgotten: bookmarks,
                            skipped: false,
                            skipped_reason: None,
                            error: None,
                        });
                    }
                    Err(e) => {
                        results.push(WorkspaceGcResult {
                            workspace_id: record.workspace_id.clone(),
                            bookmarks_forgotten: vec![],
                            skipped: false,
                            skipped_reason: None,
                            error: Some(e.to_string()),
                        });
                    }
                }
            }

            // Run the aged-unhealthy recycler so that `cube workspace gc` also
            // clears long-lived dirty/conflicted workspaces — the operator's
            // mental model is that gc cleans up dirty workspaces, not just
            // consumed bookmarks. Skipped in dry-run mode.
            let unhealthy_recycled: usize = if !dry_run {
                let gc_config = config::load_config().unwrap_or_default().unhealthy_gc;
                let max_age_secs = gc_config.max_age_secs();
                match current_epoch_s() {
                    Ok(now) => gc_aged_unhealthy_workspaces(runner, &store, database_path, now, max_age_secs, None),
                    Err(_) => 0,
                }
            } else {
                0
            };

            let total_forgotten: usize = results.iter().map(|r| r.bookmarks_forgotten.len()).sum();
            let message = if dry_run {
                format!(
                    "{} workspace(s): {} bookmark(s) would be forgotten (dry-run).",
                    results.len(),
                    total_forgotten
                )
            } else {
                format!(
                    "{} workspace(s): {} bookmark(s) forgotten, {} unhealthy workspace(s) recycled.",
                    results.len(),
                    total_forgotten,
                    unhealthy_recycled
                )
            };
            RunResult::new(
                message,
                json!({ "results": results, "unhealthy_recycled": unhealthy_recycled }),
            )
        }
        WorkspaceCommand::Goto {
            workspace,
            bookmark,
            pr,
        } => workspace_goto(database_path, runner, workspace, bookmark, pr),
        WorkspaceCommand::Rebase { bookmark, pr, no_push } => {
            workspace_rebase(&mut store, database_path, runner, bookmark, pr, no_push)
        }
        WorkspaceCommand::Push { bookmark, pr } => workspace_push(database_path, runner, bookmark, pr),
    }
}
