//! Escalation-ladder harness for the in-review merge-conflict path
//! (`docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`,
//! Layer 1 / task T4).
//!
//! The ladder restructures conflict handling from "detect → full worker"
//! into a sequence of strictly-cheaper rungs, each attempted only when the
//! cheaper one declines. This module is invoked from
//! [`crate::conflict_watch::on_conflict_detected`] *before*
//! `maybe_spawn_conflict_revision`, so a mechanical resolution retires the
//! conflict with no agent and zero tokens, and only genuinely-semantic
//! conflicts reach the worker path (rung 3).
//!
//! ## Rungs implemented here
//!
//! - **Rung 1 — engine-direct mechanical rebase.** Lease a workspace,
//!   position it on the PR, and run `cube workspace rebase` engine-side (no
//!   agent). Because jj does a real 3-way structural merge, GitHub's stale
//!   `CONFLICTING` and non-overlapping "same-file" hunks resolve for free.
//!   On `REBASED_CLEAN` the command advances and pushes the boss bookmark
//!   itself; the harness retires the attempt at rung 1 and the parent
//!   returns to Review with no worker ever spawned.
//!
//! - **Rung 0 — deterministic resolvers.** When rung 1 leaves residual
//!   conflicts, [`attempt_rung0`] feeds the residual files to the T2
//!   registry (`boss_deterministic_resolvers`). If every file resolves, it
//!   lands the result — advance the workspace's branch bookmark and push
//!   via [`CubeClient::push_resolution`] (`cube workspace push`, the "no
//!   clean verb today" gap this closes) — and retires the attempt at rung
//!   0. If any file declines, or the push fails, it falls through to rung
//!   3 exactly as an all-declined rung 1 does today. **Gated OFF** by
//!   [`RUNG0_APPLY_LIVE`] — see that constant's doc comment for why. The
//!   mechanism is fully implemented and unit-tested (`conflict_ladder_tests.rs`
//!   calls [`attempt_rung0`] directly, bypassing the gate) so a follow-up
//!   only needs to flip the constant once it's safe to.
//!
//! ## Deferred (declared in the T4 PR, still open)
//!
//! - **Escalation-on-rejection → rung 3 with findings.** When a completed
//!   resolution is rejected *post-resolution* (build gate / tripwire / AI
//!   review), the design escalates straight to rung 3 with the findings
//!   attached. For rung 1 the post-resolution gate is the PR's own CI, which
//!   the existing `ci_watch` path already observes on a later sweep; the full
//!   findings-attached escalation is gated on T9 (T2562). What the harness
//!   *does* guarantee today is the ladder's other invariant: a rung that
//!   **declines** (rebase errors, an unresolvable residual file, or a failed
//!   push) climbs — it never retries the same rung against the same state
//!   (the `conflict_resolutions` UNIQUE key dedupes identical base+head
//!   states; a genuinely new state gets a fresh attempt, bounded by the
//!   churn guard).

use boss_deterministic_resolvers::{ConflictedFile, RegistryResolution, ResolvedFile, ResolverRegistry};
use boss_protocol::FrontendEvent;

use crate::coordinator::{CubeClient, CubeWorkspaceLease, ExecutionPublisher};
use crate::merge_poller::parse_pr_number;
use crate::work::{ConflictResolution, PendingMergeCheck, WorkDb};

/// The escalation-ladder rung a resolution was produced at, recorded in
/// `conflict_resolutions.resolved_by_rung` for telemetry (T1).
const RUNG_DETERMINISTIC_RESOLVER: i64 = 0;
const RUNG_ENGINE_DIRECT_REBASE: i64 = 1;

/// Rung 0 (deterministic-resolver apply/commit/push, [`attempt_rung0`]) is
/// fully implemented and unit-tested but **must not run on the live
/// `conflict_watch` path** until T2562 (the design's T9 "result-gate" —
/// see `merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`,
/// "Composition with T2253") lands. Today the both-parents deletion
/// tripwire (T2253 P2) and the build gate only vet the rung-3 full-worker
/// path (via the `pr_review` reviewer pass); there is no standalone check
/// this harness can call to vet a mechanical rung's output before
/// auto-retiring it. Auto-pushing an unvetted resolution straight to a
/// real PR branch would violate the design's own safety model.
///
/// Deliberately a compile-time constant, not a `feature_flags`
/// debug-pane toggle: the result-gate's call shape isn't decided yet, so
/// there is nothing safe to expose as an operator-flippable switch today.
/// Flipping this to `true` — once T2562 lands and this module routes rung
/// 0's output through it — is a reviewed follow-up PR, not a runtime
/// decision.
const RUNG0_APPLY_LIVE: bool = false;

/// Result of running the mechanical rungs against a fresh conflict attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LadderOutcome {
    /// A mechanical rung produced a resolution: the attempt is retired, the
    /// parent is back in Review, and the success events are published. The
    /// caller must NOT spawn a worker revision.
    Retired,
    /// No mechanical rung produced a resolution (rung 1 unavailable, the
    /// rebase errored, or residual conflicts remain). The caller falls
    /// through to the existing worker-spawn path (rung 3). Any leased
    /// workspace has already been released.
    FellThrough,
}

/// Attempt the mechanical rungs (rung 1 today) for a freshly-detected,
/// live conflict attempt. Returns [`LadderOutcome::Retired`] when the
/// conflict was resolved with no agent, or [`LadderOutcome::FellThrough`]
/// when the caller should continue to the worker-spawn path.
///
/// Any error along the lease → position → rebase path is non-fatal: it is
/// logged and treated as "rung 1 unavailable", so a transient cube/GitHub
/// failure degrades to today's worker path rather than dropping the signal.
pub(crate) async fn try_mechanical_rungs(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube_client: &dyn CubeClient,
    candidate: &PendingMergeCheck,
    attempt: &ConflictResolution,
) -> LadderOutcome {
    let Some(pr_number) = parse_pr_number(&candidate.pr_url).filter(|n| *n > 0) else {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "conflict_ladder: could not parse PR number; skipping rung 1",
        );
        return LadderOutcome::FellThrough;
    };
    let pr_number = pr_number as u64;

    // The engine-direct rebase needs a workspace positioned on the PR. Resolve
    // the task's effective repo (per-task override beats the product default)
    // to lease against.
    let repo_remote = match work_db.resolve_repo_for_task(&candidate.work_item_id) {
        Ok(Some(url)) => url,
        Ok(None) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                "conflict_ladder: no repo_remote_url resolves for work item; skipping rung 1",
            );
            return LadderOutcome::FellThrough;
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "conflict_ladder: failed to resolve repo_remote_url; skipping rung 1",
            );
            return LadderOutcome::FellThrough;
        }
    };

    let repo = match cube_client.ensure_repo(&repo_remote).await {
        Ok(repo) => repo,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                repo_remote = %repo_remote,
                error = %format!("{err:#}"),
                "conflict_ladder: ensure_repo failed; skipping rung 1",
            );
            return LadderOutcome::FellThrough;
        }
    };

    let task_label = format!("conflict-ladder rung1 {}", candidate.work_item_id);
    let lease = match cube_client
        .lease_workspace(&repo.repo_id, &task_label, None, false, &[])
        .await
    {
        Ok(lease) => lease,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                repo_id = %repo.repo_id,
                error = %format!("{err:#}"),
                "conflict_ladder: could not lease a workspace for rung 1; falling through to worker",
            );
            return LadderOutcome::FellThrough;
        }
    };

    // From here the lease is held: run the rung and release unconditionally.
    let outcome = run_rung1_in_lease(work_db, publisher, cube_client, candidate, attempt, pr_number, &lease).await;

    if let Err(err) = cube_client.release_workspace(&lease.lease_id).await {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            lease_id = %lease.lease_id,
            ?err,
            "conflict_ladder: releasing rung-1 workspace failed (likely already released)",
        );
    }
    outcome
}

/// Position the leased workspace on the PR head, run the engine-direct
/// rebase, and act on the outcome. Split out so the caller can release the
/// lease unconditionally regardless of which branch this returns from.
async fn run_rung1_in_lease(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube_client: &dyn CubeClient,
    candidate: &PendingMergeCheck,
    attempt: &ConflictResolution,
    pr_number: u64,
    lease: &crate::coordinator::CubeWorkspaceLease,
) -> LadderOutcome {
    if let Err(err) = cube_client.goto_workspace(&lease.workspace_path, pr_number).await {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr = pr_number,
            error = %format!("{err:#}"),
            "conflict_ladder: goto_workspace failed; falling through to worker",
        );
        return LadderOutcome::FellThrough;
    }

    let rebase = match cube_client.rebase_workspace(&lease.workspace_path, pr_number).await {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr = pr_number,
                error = %format!("{err:#}"),
                "conflict_ladder: engine-direct rebase failed; falling through to worker",
            );
            return LadderOutcome::FellThrough;
        }
    };

    if rebase.clean {
        if rebase.pushed {
            // Rung 1 rebased cleanly and pushed the updated branch — the PR is
            // resolved with no agent. Retire the attempt at rung 1.
            retire_attempt_at_rung(work_db, publisher, candidate, attempt, RUNG_ENGINE_DIRECT_REBASE).await;
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr = pr_number,
                attempt_id = %attempt.id,
                "conflict_ladder: rung 1 (engine-direct rebase) resolved and pushed; auto-retired with no agent",
            );
            return LadderOutcome::Retired;
        }
        // Clean but not pushed: the harness always drives a pushing rebase, so
        // this means the push was skipped upstream. Don't retire on an
        // unpushed branch — fall through so the worker updates the PR.
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr = pr_number,
            "conflict_ladder: rung 1 rebased clean but reported unpushed; falling through to worker",
        );
        return LadderOutcome::FellThrough;
    }

    // Residual conflicts after the structural rebase are genuine overlap that
    // rung 0 (deterministic resolvers) or rung 3 (worker) must handle. Try
    // rung 0 first — gated off live by RUNG0_APPLY_LIVE (see its doc comment)
    // until T2562's result-gate lands; `attempt_rung0` itself decides whether
    // every residual file actually resolves.
    if RUNG0_APPLY_LIVE
        && attempt_rung0(
            work_db,
            publisher,
            cube_client,
            candidate,
            attempt,
            lease,
            &rebase.conflicted_files,
        )
        .await
            == LadderOutcome::Retired
    {
        return LadderOutcome::Retired;
    }

    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr = pr_number,
        attempt_id = %attempt.id,
        residual_conflicts = rebase.conflicted_files.len(),
        "conflict_ladder: rung 1 left residual conflicts; climbing to worker (rung 3)",
    );
    LadderOutcome::FellThrough
}

/// Rung 0: feed rung-1's residual conflicted files to the deterministic-
/// resolver registry (T2, `boss_deterministic_resolvers`). If every file
/// resolves, land the result — advance the workspace's branch bookmark and
/// push via [`CubeClient::push_resolution`] (`cube workspace push`) — and
/// retire the attempt at rung 0. Returns [`LadderOutcome::FellThrough`]
/// when any file declines or the push fails, leaving the attempt untouched
/// so the caller's existing rung-3 fallback takes over exactly as it does
/// today.
///
/// A free function (rather than inlined into [`run_rung1_in_lease`]) so
/// this rung's actual mechanics are unit-testable in isolation from the
/// [`RUNG0_APPLY_LIVE`] gate at its one live call site.
///
/// Re-derives the PR number from `candidate.pr_url` rather than taking it
/// as a parameter (`try_mechanical_rungs` already validated it parses
/// before leasing a workspace) — one fewer argument keeps this under
/// clippy's `too_many_arguments` limit.
pub(crate) async fn attempt_rung0(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube_client: &dyn CubeClient,
    candidate: &PendingMergeCheck,
    attempt: &ConflictResolution,
    lease: &CubeWorkspaceLease,
    residual_paths: &[String],
) -> LadderOutcome {
    let Some(pr_number) = parse_pr_number(&candidate.pr_url).filter(|n| *n > 0) else {
        // Unreachable via the live call site (try_mechanical_rungs already
        // parsed this successfully before leasing), but attempt_rung0 is
        // also called directly by tests, so this stays a clean decline
        // rather than a panic.
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "conflict_ladder: rung 0 could not parse PR number; declining",
        );
        return LadderOutcome::FellThrough;
    };
    let pr_number = pr_number as u64;

    // Rung 1's `conflicted_files` are bare paths (from `jj resolve --list`),
    // not the merge-tree-derived diagnosis `boss_conflict_diagnosis` produces
    // elsewhere — there is no marker-tree available here, so `shape` is the
    // same generic "content" the producer-rebase diagnosis path uses for the
    // same reason (see `conflict_res.rs`).
    let files: Vec<ConflictedFile> = residual_paths
        .iter()
        .map(|path| ConflictedFile {
            path: path.clone(),
            marker_count: None,
            shape: "content".to_owned(),
        })
        .collect();

    let registry = ResolverRegistry::with_builtins();
    let resolved: Vec<ResolvedFile> = match registry.resolve_all(&lease.workspace_path, &files).await {
        RegistryResolution::AllResolved(resolved) => resolved,
        RegistryResolution::Declined { resolved, declined } => {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr = pr_number,
                resolved = resolved.len(),
                declined = declined.len(),
                "conflict_ladder: rung 0 declined (not every residual file has a resolver); climbing to worker",
            );
            return LadderOutcome::FellThrough;
        }
    };

    if let Err(err) = cube_client.push_resolution(&lease.workspace_path, pr_number).await {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr = pr_number,
            error = %format!("{err:#}"),
            "conflict_ladder: rung 0 resolved every residual file but the push failed; climbing to worker",
        );
        return LadderOutcome::FellThrough;
    }

    retire_attempt_at_rung(work_db, publisher, candidate, attempt, RUNG_DETERMINISTIC_RESOLVER).await;
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr = pr_number,
        attempt_id = %attempt.id,
        resolved_files = resolved.len(),
        "conflict_ladder: rung 0 (deterministic resolvers) resolved and pushed; auto-retired with no agent",
    );
    LadderOutcome::Retired
}

/// Retire a `conflict_resolutions` attempt a mechanical rung (0 or 1)
/// resolved: clear the upfront `blocked: merge_conflict` flip back to
/// `in_review`, mark the attempt `succeeded` stamped `resolved_by_rung =
/// rung`, clear the in-flight signal, and publish the success events.
/// Mirrors the retire half of [`crate::conflict_watch::on_resolved`] for
/// the "we resolved it ourselves" case, reusing the same `WorkDb`
/// primitives.
async fn retire_attempt_at_rung(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    attempt: &ConflictResolution,
    rung: i64,
) {
    // The parent was flipped to `blocked: merge_conflict` upfront by
    // `on_conflict_detected`; clear it back to `in_review`. The WHERE guard
    // only clears engine-owned rows, so a human-moved row is left alone.
    let task_transitioned = match work_db.clear_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
    {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                rung,
                ?err,
                "conflict_ladder: failed to clear block on retire",
            );
            false
        }
    };

    // The pushed head sha is not returned by the rebase/push payload; leave
    // `head_sha_after` for a later probe to fill. The rung stamp is the point.
    if let Err(err) = work_db.mark_conflict_resolution_succeeded_at_rung(&attempt.id, None, rung) {
        tracing::warn!(
            attempt_id = %attempt.id,
            rung,
            ?err,
            "conflict_ladder: failed to mark attempt succeeded",
        );
    }

    // Clear the in-flight merge_conflict signal so `maybe_clear_blocked` does
    // not re-fire on the next probe. No-op when no signal row exists.
    if let Err(err) = work_db.clear_merge_conflict_signal_only(&candidate.work_item_id) {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            rung,
            ?err,
            "conflict_ladder: failed to clear in-flight signal on retire",
        );
    }

    publisher
        .publish_frontend_event_on_product(
            &candidate.product_id,
            FrontendEvent::ConflictResolutionSucceeded {
                product_id: candidate.product_id.clone(),
                work_item_id: candidate.work_item_id.clone(),
                attempt_id: attempt.id.clone(),
                pr_url: candidate.pr_url.clone(),
            },
        )
        .await;

    // Broadcast the parent status change only when it actually transitioned
    // (blocked → in_review). Mirrors `on_resolved`.
    if task_transitioned {
        publisher
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                "merge_conflict_resolved",
            )
            .await;
    }
}

#[cfg(test)]
#[path = "conflict_ladder_tests.rs"]
mod tests;
