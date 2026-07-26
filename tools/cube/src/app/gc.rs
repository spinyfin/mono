//! Garbage collection: consumed bookmarks, closed-PR bookmarks, aged
//! unhealthy workspaces, stale logs, and the background pool GC trigger.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use git_utils::pr_bookmark;
use git_utils::repo_slug::parse_github_remote;

use crate::command_runner::{CommandRunner, RealCommandRunner};
use crate::metadata::{WorkspaceHealth, WorkspaceRecord, WorkspaceState};
use crate::store::{EffectiveState, Store, WorkspaceListFilter};
use crate::{audit, config, paths};

use crate::app::errors::Result;
use crate::app::health::probe_workspace_reuse;
use crate::app::jj::{run_jj, run_jj_network, workspace_path_exists};
use crate::app::reconcile::reconcile_free_workspace_health;
use crate::app::reset::reset_workspace_after_fetch_within;
use crate::app::salvage::{
    attributed_holder, attributed_task, discard_salvage_record, gc_aged_salvage_records, salvage_workspace,
};
use crate::app::util::current_epoch_s;

/// Pool-wide gc runs at most once per 24 hours (when there is no known
/// backlog), triggered from `cube workspace lease`.
pub(super) const AUTO_GC_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// Stamped on COMPLETION of a pool GC pass; gates the throttle above.
pub(super) const POOL_GC_LAST_AT_KEY: &str = "last_pool_gc_at";
/// Stamped before running the GC pass, to prevent two overlapping `cube
/// workspace lease` invocations from both running a pass at once. The pass
/// itself now runs synchronously (see `maybe_trigger_pool_gc`), bounded by
/// POOL_GC_TIME_BUDGET and wrapped in `catch_unwind`, so it always clears/
/// advances this key before the triggering process exits — a started-but-
/// never-completed pass should no longer be possible. This timeout is kept
/// only as a safety net against a process that dies by external means (e.g.
/// SIGKILL) mid-pass.
pub(super) const POOL_GC_STARTED_AT_KEY: &str = "last_pool_gc_started_at";
pub(super) const POOL_GC_IN_PROGRESS_TIMEOUT_SECS: i64 = 5 * 60; // 5 minutes

/// Wall-clock budget for a pool GC pass triggered automatically from `cube
/// workspace lease`. `lease` is a short-lived CLI process — it used to spawn
/// GC on a detached `std::thread` and exit immediately after, which killed
/// the thread mid-pass on essentially every invocation (the pass never got
/// to stamp completion). Running synchronously fixes that, but a lease call
/// can't be allowed to block on an unbounded pass, so per-workspace GC work
/// checks this deadline before starting each new unit of work and stops
/// early — cleanly, not by being killed — once it's exceeded. The explicit,
/// operator-invoked `cube workspace gc` verb is unbounded by design (`None`
/// deadline) since it isn't on the lease hot path.
const POOL_GC_TIME_BUDGET: Duration = Duration::from_secs(20);

/// How soon a pool GC pass that hit POOL_GC_TIME_BUDGET before clearing all
/// aged-unhealthy candidates is eligible to run again, instead of waiting the
/// full AUTO_GC_INTERVAL_SECS. Keeps a backlog draining across many short,
/// bounded passes rather than stalling for a day once there's more work than
/// one budget allows.
pub(super) const POOL_GC_BACKLOG_RETRY_SECS: i64 = 5 * 60;

/// List and optionally forget consumed `boss/exec_*` bookmarks and closed/merged
/// `pr/<n>` bookmarks in a workspace.
///
/// A `boss/exec_*` bookmark is "consumed" when its tip is reachable from `main`
/// (`bookmarks(glob:"boss/exec_*") & ::main`). A `pr/<n>` bookmark is eligible
/// for GC when its corresponding GitHub PR is in the MERGED or CLOSED state
/// (resolved via `gh pr view`; skipped silently when offline).
///
/// If `do_fetch` is true, runs `jj git fetch` first so `::main` reflects the
/// latest merged PRs. If `dry_run` is true, lists what would be forgotten
/// without acting.
///
/// Returns the names of bookmarks forgotten (or that would be forgotten on
/// dry-run). Failures are propagated to the caller; release-path callers
/// should treat them as warnings.
pub(super) fn gc_workspace_bookmarks(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    do_fetch: bool,
    dry_run: bool,
) -> Result<Vec<String>> {
    if do_fetch {
        run_jj_network(
            runner,
            database_path,
            &RealCommandRunner::invocation(workspace_path, "jj", &["git", "fetch"]),
        )?;
    }

    let output = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            workspace_path,
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"boss/exec_*\") & ::main",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
        ),
    )?;

    let mut bookmarks: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        output
            .split_whitespace()
            .filter(|s| s.starts_with("boss/exec_") && !s.contains('@'))
            .filter(|s| seen.insert(s.to_string()))
            .map(str::to_string)
            .collect()
    };

    // Also sweep pr/<n> bookmarks whose GitHub PR is closed or merged.
    bookmarks.extend(gc_collect_closed_pr_bookmarks(runner, database_path, workspace_path));

    if bookmarks.is_empty() || dry_run {
        return Ok(bookmarks);
    }

    let mut args: Vec<&str> = vec!["bookmark", "forget"];
    let bookmark_refs: Vec<&str> = bookmarks.iter().map(String::as_str).collect();
    args.extend_from_slice(&bookmark_refs);
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &args),
    )?;

    Ok(bookmarks)
}

/// Collect local `pr/<n>` bookmarks in `workspace_path` whose GitHub PR is
/// MERGED or CLOSED. Returns an empty list when offline, the workspace has no
/// GitHub remote, or there are no `pr/*` bookmarks. Failures from `jj` or
/// `gh` are swallowed so this best-effort sweep never blocks the caller.
pub(super) fn gc_collect_closed_pr_bookmarks(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Vec<String> {
    // Resolve the GitHub owner/repo slug from the workspace's jj remotes.
    let remote_output = match runner.run(&RealCommandRunner::invocation(
        workspace_path,
        "jj",
        &["git", "remote", "list"],
    )) {
        Ok(out) => out,
        Err(_) => return vec![],
    };
    let (_remote_name, owner_repo) = match parse_github_remote(&remote_output) {
        Some(r) => r,
        None => return vec![],
    };

    // Find all local pr/* bookmarks in the workspace.
    let bookmark_output = match run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            workspace_path,
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"pr/*\")",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
        ),
    ) {
        Ok(out) => out,
        Err(_) => return vec![],
    };

    let pr_bookmarks: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        bookmark_output
            .split_whitespace()
            .filter(|s| pr_bookmark::is_pr_bookmark(s) && !s.contains('@'))
            .filter(|s| seen.insert(s.to_string()))
            .map(str::to_string)
            .collect()
    };

    if pr_bookmarks.is_empty() {
        return vec![];
    }

    // For each pr/<n> bookmark, query GitHub for the PR state. Skip silently
    // on network/auth failures so GC degrades gracefully when offline.
    pr_bookmarks
        .into_iter()
        .filter(|bookmark| {
            let Some(pr_num) = bookmark.strip_prefix("pr/") else {
                return false;
            };
            let state = match runner.run(&RealCommandRunner::invocation(
                workspace_path,
                "gh",
                &[
                    "pr",
                    "view",
                    pr_num,
                    "-R",
                    &owner_repo,
                    "--json",
                    "state",
                    "--jq",
                    ".state",
                ],
            )) {
                Ok(out) => out,
                Err(_) => return false,
            };
            matches!(state.trim(), "MERGED" | "CLOSED")
        })
        .collect()
}

/// Cheap, read-only check for whether any aged-unhealthy candidate currently
/// exists that `gc_aged_unhealthy_workspaces` would pick up — used to decide
/// whether a completed-but-partial pass should be retried after
/// POOL_GC_BACKLOG_RETRY_SECS instead of waiting a full day.
///
/// The candidate predicate here must mirror `gc_aged_unhealthy_workspaces`'s
/// exactly, including `Quarantined` and its "no `unhealthy_since` means treat
/// as old" allowance. It previously matched only `Dirty`/`Conflicted`, on the
/// reasoning that a quarantined workspace holding unpushed work stayed
/// quarantined forever and would therefore pin the retry interval at 5
/// minutes permanently. Retention now expires — age plus a successful salvage
/// reclaims exactly those rows — so that reasoning no longer holds, and the
/// mismatch had a real cost: with a backlog that is mostly quarantined, a
/// partial 20-second pass stamps completion, this returns `false`, and the
/// next auto GC waits AUTO_GC_INTERVAL_SECS (24h) rather than
/// POOL_GC_BACKLOG_RETRY_SECS (5m). The free pool stays withheld far longer
/// than the 24h retention TTL implies — the "effective free collapses while
/// GC sleeps" failure this whole change exists to fix.
///
/// This deliberately does not distinguish "will succeed" from "will be tried
/// and refused". A candidate whose salvage keeps failing genuinely is
/// outstanding work, and retrying it every POOL_GC_BACKLOG_RETRY_SECS is the
/// correct answer for the rest of the backlog it is sharing a pass with;
/// telling the two apart needs the live probe, which is exactly the cost this
/// check exists to avoid.
pub(super) fn pool_gc_has_aged_unhealthy_backlog(store: &Store, now_epoch_s: i64) -> Result<bool> {
    let max_age_secs = config::load_config().unwrap_or_default().unhealthy_gc.max_age_secs();
    let threshold_epoch_s = now_epoch_s.saturating_sub(max_age_secs);
    let records = store.list_workspaces_filtered(&WorkspaceListFilter::default())?;
    Ok(records
        .into_iter()
        .any(|record| aged_unhealthy_since(&record, threshold_epoch_s).is_some()))
}

/// The shared candidate predicate for the aged-unhealthy passes: `Some(since)`
/// when `record` is a free workspace whose unhealthy clock started at or
/// before `threshold_epoch_s`, `None` when it is not a candidate at all.
///
/// Factored out so `pool_gc_has_aged_unhealthy_backlog` (which decides *when*
/// the next pass runs) and `gc_aged_unhealthy_workspaces` (which does the
/// work) cannot drift apart again.
fn aged_unhealthy_since(record: &WorkspaceRecord, threshold_epoch_s: i64) -> Option<i64> {
    if record.state == WorkspaceState::Leased {
        return None;
    }
    if !matches!(
        record.health_status,
        Some(WorkspaceHealth::Dirty) | Some(WorkspaceHealth::Conflicted) | Some(WorkspaceHealth::Quarantined)
    ) {
        return None;
    }
    // A quarantined row written before the timestamp was stamped on that path
    // has no age to compare; treat it as old rather than stranding it forever
    // — the reuse probe is what actually decides whether it is safe to touch.
    // Dirty/conflicted rows keep the strict behaviour.
    let unhealthy_since = match (
        record.unhealthy_since_epoch_s,
        record.health_status == Some(WorkspaceHealth::Quarantined),
    ) {
        (Some(since), _) => since,
        (None, true) => 0,
        (None, false) => return None,
    };
    (unhealthy_since <= threshold_epoch_s).then_some(unhealthy_since)
}

/// Trigger a pool GC pass, throttled so it does not run on every lease.
///
/// Two metadata keys guard the trigger:
/// - `last_pool_gc_at` (POOL_GC_LAST_AT_KEY): stamped on completion; gates
///   the throttle (AUTO_GC_INTERVAL_SECS normally, POOL_GC_BACKLOG_RETRY_SECS
///   when a backlog of aged-unhealthy workspaces is known to remain).
/// - `last_pool_gc_started_at` (POOL_GC_STARTED_AT_KEY): stamped here before
///   the pass runs, to prevent two overlapping `cube workspace lease`
///   invocations from both running a pass at once.
///
/// The pass itself runs synchronously on this call — not on a detached
/// `std::thread::spawn` — because `cube workspace lease` is a short-lived CLI
/// process that used to exit (killing the thread) before the pass could ever
/// finish. It is bounded by POOL_GC_TIME_BUDGET so a large backlog cannot
/// stall lease dispatch, and wrapped in `catch_unwind` so a panic inside it
/// cannot leave `last_pool_gc_started_at` permanently ahead of
/// `last_pool_gc_at`.
pub(super) fn maybe_trigger_pool_gc(store: &mut Store, database_path: Option<&Path>, now_epoch_s: i64) -> Result<()> {
    // Skip if a pass is already in progress (started within the safety-net timeout).
    let last_started = store.get_pool_metadata_i(POOL_GC_STARTED_AT_KEY)?;
    if last_started.is_some_and(|t| (now_epoch_s - t) < POOL_GC_IN_PROGRESS_TIMEOUT_SECS) {
        return Ok(());
    }
    // Skip if a pass completed recently — how recently depends on whether
    // there is known backlog left over from a prior bounded pass.
    if let Some(last_completed) = store.get_pool_metadata_i(POOL_GC_LAST_AT_KEY)? {
        let interval = if pool_gc_has_aged_unhealthy_backlog(store, now_epoch_s).unwrap_or(true) {
            POOL_GC_BACKLOG_RETRY_SECS
        } else {
            AUTO_GC_INTERVAL_SECS
        };
        if (now_epoch_s - last_completed) < interval {
            return Ok(());
        }
    }

    store.set_pool_metadata_i(POOL_GC_STARTED_AT_KEY, now_epoch_s)?;
    let db_path_owned = database_path.map(Path::to_path_buf);
    let deadline = Instant::now() + POOL_GC_TIME_BUDGET;
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_pool_gc_background(db_path_owned, deadline);
    })) {
        eprintln!("cube: auto gc: pass panicked: {panic:?}");
    }
    // Always stamp completion here — whether the pass above finished all its
    // work, stopped early at the deadline, or panicked — so a broken pass can
    // never leave `last_pool_gc_started_at` stuck ahead of `last_pool_gc_at`.
    let _ = store
        .set_pool_metadata_i(POOL_GC_LAST_AT_KEY, now_epoch_s)
        .map_err(|e| eprintln!("cube: auto gc: failed to stamp completion: {e}"));
    Ok(())
}

pub(super) fn run_pool_gc_background(database_path: Option<std::path::PathBuf>, deadline: Instant) {
    let store = match database_path.as_deref() {
        Some(p) => Store::open_at(p),
        None => Store::open_default(),
    };
    let Ok(store) = store else {
        eprintln!("cube: auto gc: failed to open store");
        return;
    };
    let runner = RealCommandRunner;

    // Run the aged-unhealthy recycler first so it genuinely gets first claim
    // on the time budget — it is the part that actually reclaims disk space
    // from stuck-dirty workspaces, and it must not be starved by the
    // reconcile pass below (which only refreshes cached health labels, a
    // less time-critical repair). It is bounded by `deadline` itself, so a
    // large backlog cannot consume more than its share.
    let gc_config = config::load_config().unwrap_or_default().unhealthy_gc;
    let max_age_secs = gc_config.max_age_secs();
    if let Ok(now) = current_epoch_s() {
        gc_aged_unhealthy_workspaces(
            &runner,
            &store,
            database_path.as_deref(),
            now,
            max_age_secs,
            Some(deadline),
        );
    }

    if Instant::now() >= deadline {
        eprintln!(
            "cube: auto gc: time budget exhausted after aged-unhealthy pass, deferring reconcile/log/bookmark gc"
        );
        return;
    }

    // Reconcile stale health cache: re-check dirty/conflicted workspaces
    // against their on-disk state and promote any that have recovered to
    // clean. Runs after the aged-unhealthy pass (which already reset whatever
    // it could) and is itself bounded by the same deadline so it cannot eat
    // into the remaining budget meant for the log/bookmark sweep below.
    let health_report = reconcile_free_workspace_health(
        &runner,
        &store,
        database_path.as_deref(),
        None,
        None,
        false,
        Some(deadline),
    );
    if !health_report.promoted_to_clean.is_empty() {
        eprintln!(
            "cube: auto gc: promoted {} workspace(s) from dirty/conflicted to clean",
            health_report.promoted_to_clean.len(),
        );
    }

    if Instant::now() >= deadline {
        eprintln!("cube: auto gc: time budget exhausted after reconcile pass, deferring log/bookmark gc");
        return;
    }

    gc_stale_workspace_logs(&store);

    // Salvage records are themselves retained, not kept forever — the failure
    // mode this whole change is about does not get to reappear one level up.
    if let Ok(now) = current_epoch_s() {
        let removed = gc_aged_salvage_records(database_path.as_deref(), now);
        if removed > 0 {
            eprintln!("cube: auto gc: removed {removed} salvage record(s) past their retention window");
        }
    }

    // Sweep consumed bookmarks from every non-leased workspace. Each workspace
    // runs jj git fetch; a broken/unreachable remote times out after
    // network_cmd_timeout() and is skipped so one slow workspace cannot block
    // the rest of the pass. Bounded by the same deadline as the aged-unhealthy
    // pass above, checked before starting each workspace.
    let records = match store.list_workspaces_filtered(&WorkspaceListFilter::default()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cube: auto gc: failed to list workspaces: {e}");
            return;
        }
    };
    let mut budget_exhausted = false;
    for record in &records {
        if record.state == WorkspaceState::Leased {
            continue;
        }
        if !workspace_path_exists(record) {
            continue;
        }
        if Instant::now() >= deadline {
            budget_exhausted = true;
            break;
        }
        if let Err(e) = gc_workspace_bookmarks(&runner, database_path.as_deref(), &record.workspace_path, true, false) {
            eprintln!("cube: auto gc: {}: {e}", record.workspace_id,);
        }
    }
    if budget_exhausted {
        eprintln!("cube: auto gc: time budget exhausted, deferred remaining bookmark gc to next pass");
    }
}

/// How many quarantined workspaces one lease call will probe before giving up
/// and provisioning a fresh one instead.
///
/// Each probe costs a `jj git fetch` plus one or two `jj log`s against a
/// workspace we may not end up using, and the whole scan runs under the
/// per-repo lock, so it is bounded rather than sweeping the entire quarantine
/// set. A truncated scan is recorded in `workspace.quarantine_reclaim_scan`
/// (`scanned` vs `available`), never dropped silently — the next lease that
/// would otherwise mint picks up where this one stopped.
const QUARANTINE_RECLAIM_MAX_PROBES: usize = 4;

/// Try to return a quarantined workspace to the pool instead of minting a new
/// one, and hand back its id if that succeeds.
///
/// Quarantine is set by the dirty-reclaim guard when it refuses a destructive
/// reset. Before this existed, that marker was permanent: no GC pass cleared
/// it and the only escape was an operator running `cube workspace
/// force-release`, so every refusal permanently removed a workspace from the
/// pool. Reclaim is *verified*, never blind — the guard's own probe is re-run
/// and the quarantine is cleared only when `@` holds nothing that would be
/// left behind (it is empty, or a remote already has it). A workspace whose
/// `@` still holds an unpushed working copy stays quarantined, which is the
/// whole point of the marker.
///
/// Never fails a lease: any probe error skips that workspace and the caller
/// falls through to provisioning.
pub(super) fn reclaim_quarantined_workspace(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    repo: &str,
    main_branch: &str,
    exclude: &[String],
    deadline: Option<Instant>,
) -> Result<Option<String>> {
    let quarantined = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: Some(repo),
        effective_state: Some(EffectiveState::FreeQuarantined),
        ..Default::default()
    })?;

    let available: Vec<&WorkspaceRecord> = quarantined
        .iter()
        .filter(|record| !exclude.contains(&record.workspace_id) && workspace_path_exists(record))
        .collect();

    let mut scanned = 0usize;
    let mut reclaimed: Option<String> = None;
    let mut timed_out = false;
    for record in available.iter().take(QUARANTINE_RECLAIM_MAX_PROBES) {
        // Probe count alone does not bound cost: each probe includes a
        // `jj git fetch`, so on a slow remote four of them can outlast the
        // caller's whole lease timeout. Stop at the deadline and let the
        // caller provision instead — same trade as the health scan.
        if deadline.is_some_and(|d| Instant::now() >= d) {
            timed_out = true;
            break;
        }
        scanned += 1;
        let status = match probe_workspace_reuse(runner, database_path, &record.workspace_path, main_branch, deadline) {
            Ok(status) => status,
            Err(error) => {
                eprintln!(
                    "cube: quarantine reclaim: {}: probe failed, leaving quarantined: {error}",
                    record.workspace_id,
                );
                audit!(
                    database_path,
                    "workspace.quarantine_reclaim_error",
                    repo = repo,
                    workspace_id = record.workspace_id,
                    error = error.to_string(),
                );
                continue;
            }
        };

        if !status.is_reusable() {
            audit!(
                database_path,
                "workspace.quarantine_reclaim_refused",
                repo = repo,
                workspace_id = record.workspace_id,
                head_change_id = status.head_change_id(),
                head_is_empty = status.head_is_empty(),
                head_parent_bookmarks = status.head_parent_bookmarks(),
                unpushed_commits = status.unpushed_summary(),
            );
            continue;
        }

        if !store.clear_workspace_quarantine(repo, &record.workspace_id)? {
            // Claimed or re-marked between the list and here; leave it be.
            continue;
        }
        audit!(
            database_path,
            "workspace.quarantine_cleared",
            repo = repo,
            workspace_id = record.workspace_id,
            source = "lease_reclaim",
            reuse_reason = status.reuse_reason(),
            head_change_id = status.head_change_id(),
            head_parent_bookmarks = status.head_parent_bookmarks(),
        );
        reclaimed = Some(record.workspace_id.clone());
        break;
    }

    audit!(
        database_path,
        "workspace.quarantine_reclaim_scan",
        repo = repo,
        available = available.len(),
        scanned = scanned,
        truncated = available.len() > scanned,
        timed_out = timed_out,
        reclaimed = reclaimed.as_deref(),
    );

    Ok(reclaimed)
}

/// During pool GC, reclaim any non-leased free workspace that has been
/// continuously `dirty`, `conflicted` or `quarantined` for longer than
/// `max_age_secs` — the retention TTL. Returns the number of workspaces
/// successfully recycled.
///
/// ## Retention is bounded, and expiry is not destructive
///
/// A workspace released with unpushed work is withheld from the pool so the
/// work stays reachable. That protection had no clock: nothing ever decided
/// the work was no longer wanted, so "preserved" meant "forever", and the pool
/// filled with permanently ineligible entries that every lease still paid to
/// consider. This function is the clock.
///
/// Expiring retention must never mean losing work, so every candidate is
/// probed before it is touched, and one that still holds work no remote has is
/// **salvaged to a durable record first** (see [`crate::app`]'s salvage
/// module: manifest, per-commit git-format patches, outside the workspace and
/// outside the jj store). Only a successful salvage licenses the reset. A
/// salvage that fails leaves the workspace retained and tries again next pass
/// — a stuck workspace is a far smaller problem than a lost afternoon.
///
/// `quarantined` rows reach the same place by the same route. They exist
/// because the dirty-reclaim guard refused a reset, so age alone was never
/// enough to clear them; now age plus a successful salvage is. Age still gates
/// the attempt either way, giving an operator a window to inspect a fresh
/// quarantine before cube touches it.
///
/// `deadline`, when set, is checked before starting work on each candidate;
/// once passed, remaining candidates are left for the next pass rather than
/// starting new (potentially slow, network-bound) reset work. Pass `None`
/// for an unbounded run — used by the explicit, operator-invoked `cube
/// workspace gc` verb, which isn't on the lease dispatch hot path.
pub(super) fn gc_aged_unhealthy_workspaces(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    now_epoch_s: i64,
    max_age_secs: i64,
    deadline: Option<Instant>,
) -> usize {
    let records = match store.list_workspaces_filtered(&WorkspaceListFilter::default()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cube: unhealthy gc: failed to list workspaces: {e}");
            return 0;
        }
    };

    let threshold_epoch_s = now_epoch_s.saturating_sub(max_age_secs);
    let mut recycled: usize = 0;

    for record in records {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("cube: unhealthy gc: time budget exhausted, deferring remaining candidates to next pass");
            break;
        }
        let Some(unhealthy_since) = aged_unhealthy_since(&record, threshold_epoch_s) else {
            continue;
        };
        let is_quarantined = record.health_status == Some(WorkspaceHealth::Quarantined);
        if !workspace_path_exists(&record) {
            continue;
        }

        // Re-check state: skip if the workspace was claimed between the list
        // and this point.
        let current_state = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some(&record.repo),
                workspace_id: Some(&record.workspace_id),
                ..Default::default()
            })
            .ok()
            .and_then(|mut v| v.pop())
            .map(|r| r.state);
        if current_state != Some(WorkspaceState::Free) {
            eprintln!(
                "cube: unhealthy gc: {} was claimed mid-pass, skipping",
                record.workspace_id,
            );
            continue;
        }

        let main_branch = match store.get_repo(&record.repo).ok().flatten() {
            Some(r) => r.main_branch,
            None => {
                eprintln!(
                    "cube: unhealthy gc: {}: repo {} not found, skipping",
                    record.workspace_id, record.repo,
                );
                continue;
            }
        };

        let prior_health = record.health_status.map(|h| h.as_str()).unwrap_or("unknown");
        let age_secs = now_epoch_s.saturating_sub(unhealthy_since);

        // Look before touching anything. The probe fetches first, so the
        // "does any remote already have this?" verdict is current rather than
        // a stale local view — the same reason the release-time guard fetches.
        //
        // `deadline` is threaded all the way into the subprocess timeouts, not
        // merely consulted before the candidate is picked: the fetch inside
        // this probe is 120s per attempt with two retries behind it, so one
        // slow remote could otherwise hold a lease for several minutes and
        // blow both this pass's 20s budget and the engine's 90s lease timeout.
        let status = match probe_workspace_reuse(runner, database_path, &record.workspace_path, &main_branch, deadline)
        {
            Ok(status) => status,
            Err(e) => {
                eprintln!(
                    "cube: retention: {}: reuse probe failed, leaving retained: {e}",
                    record.workspace_id,
                );
                audit!(
                    database_path,
                    "workspace.retention_probe_error",
                    repo = record.repo,
                    workspace_id = record.workspace_id,
                    prior_health = prior_health,
                    age_secs = age_secs,
                    error = e.to_string(),
                );
                continue;
            }
        };

        // Anything the workspace holds that no remote has gets salvaged before
        // the reset. A failed salvage is a hard stop for this workspace: the
        // whole basis for bounding retention is that expiry cannot lose work.
        // "Failed" includes any capture that cannot be proven complete — an
        // empty log after a probe that refused reuse, or a stack larger than
        // the export cap — see `salvage_workspace`.
        let mut salvaged_to: Option<PathBuf> = None;
        if !status.is_reusable() {
            match salvage_workspace(runner, database_path, &record, &main_branch, now_epoch_s, deadline) {
                Ok(path) => {
                    let path_str = path.display().to_string();
                    eprintln!(
                        "cube: retention: {} held unpushed work; salvaged to {path_str} before reclaiming",
                        record.workspace_id,
                    );
                    audit!(
                        database_path,
                        "workspace.retention_salvaged",
                        repo = record.repo,
                        workspace_id = record.workspace_id,
                        prior_health = prior_health,
                        prior_holder = attributed_holder(&record),
                        prior_task = attributed_task(&record),
                        age_secs = age_secs,
                        head_change_id = status.head_change_id(),
                        unpushed_commits = status.unpushed_summary(),
                        salvage_path = path_str,
                    );
                    salvaged_to = Some(path);
                }
                Err(e) => {
                    eprintln!(
                        "cube: retention: {}: salvage failed, leaving retained rather than \
                         reclaiming: {e}",
                        record.workspace_id,
                    );
                    audit!(
                        database_path,
                        "workspace.retention_salvage_failed",
                        repo = record.repo,
                        workspace_id = record.workspace_id,
                        prior_health = prior_health,
                        age_secs = age_secs,
                        head_change_id = status.head_change_id(),
                        unpushed_commits = status.unpushed_summary(),
                        error = e.to_string(),
                    );
                    continue;
                }
            }
        } else if is_quarantined {
            audit!(
                database_path,
                "workspace.quarantine_cleared",
                repo = record.repo,
                workspace_id = record.workspace_id,
                source = "unhealthy_gc",
                reuse_reason = status.reuse_reason(),
                head_change_id = status.head_change_id(),
                head_parent_bookmarks = status.head_parent_bookmarks(),
                age_secs = age_secs,
            );
        }

        // `probe_workspace_reuse` already fetched, so finish the reset from
        // there rather than paying a second network round trip per workspace
        // in a time-budgeted pass. Bounded by the same deadline as everything
        // else in this candidate's turn.
        if let Err(e) = reset_workspace_after_fetch_within(
            runner,
            database_path,
            &record.workspace_path,
            &main_branch,
            None,
            deadline,
        ) {
            eprintln!("cube: retention: {}: reset failed: {e}", record.workspace_id,);
            // The reclaim this salvage was taken for did not happen, so the
            // live workspace is still the source of truth and still retained.
            // Drop the copy: keeping it would have the next pass write a
            // second one, and repeated reset failures would multiply full
            // copies of the same stack until the salvage TTL.
            if let Some(path) = &salvaged_to {
                discard_salvage_record(path);
                audit!(
                    database_path,
                    "workspace.retention_salvage_discarded",
                    repo = record.repo,
                    workspace_id = record.workspace_id,
                    salvage_path = path.display().to_string(),
                    reason = "reset_failed",
                    error = e.to_string(),
                );
            }
            continue;
        }

        match store.gc_clear_workspace_unhealthy_state(&record.repo, &record.workspace_id) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!(
                    "cube: unhealthy gc: {}: claimed between reset and store update",
                    record.workspace_id,
                );
                continue;
            }
            Err(e) => {
                eprintln!(
                    "cube: unhealthy gc: {}: failed to clear store state: {e}",
                    record.workspace_id,
                );
                continue;
            }
        }

        let salvaged_to = salvaged_to.map(|p| p.display().to_string());
        audit!(
            database_path,
            "workspace.unhealthy_gc_reset",
            workspace_id = record.workspace_id,
            repo = record.repo,
            prior_health = prior_health,
            prior_holder = attributed_holder(&record),
            prior_task = attributed_task(&record),
            prior_release_reason = record.last_release_reason.as_deref(),
            unhealthy_since_epoch_s = unhealthy_since,
            age_secs = age_secs,
            max_age_secs = max_age_secs,
            salvage_path = salvaged_to.as_deref(),
        );

        eprintln!(
            "cube: retention: reclaimed {} (retained {} for {}s, TTL {}s{})",
            record.workspace_id,
            prior_health,
            age_secs,
            max_age_secs,
            salvaged_to
                .as_deref()
                .map(|p| format!("; salvaged to {p}"))
                .unwrap_or_default(),
        );
        recycled += 1;
    }
    recycled
}

fn gc_stale_workspace_logs(store: &Store) {
    let data_dir = match paths::data_dir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let logs_dir = paths::workspace_logs_dir_in(&data_dir);
    if !logs_dir.exists() {
        return;
    }
    let entries = match fs::read_dir(&logs_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cube: workspace logs gc: failed to read {}: {e}", logs_dir.display());
            return;
        }
    };
    let active_workspaces = match store.list_workspaces_filtered(&WorkspaceListFilter::default()) {
        Ok(w) => w
            .iter()
            .map(|r| r.workspace_id.clone())
            .collect::<std::collections::HashSet<_>>(),
        Err(e) => {
            eprintln!("cube: workspace logs gc: failed to list workspaces: {e}");
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let workspace_id = match path.file_name().and_then(|n| n.to_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        if !active_workspaces.contains(&workspace_id)
            && let Err(e) = fs::remove_dir_all(&path)
        {
            eprintln!("cube: workspace logs gc: failed to remove {}: {e}", path.display());
        }
    }
}
