//! Escalation-ladder harness for the in-review merge-conflict path
//! (`docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`,
//! Layer 1 / tasks T4 + T6).
//!
//! The ladder restructures conflict handling from "detect → full worker"
//! into a sequence of strictly-cheaper rungs, each attempted only when the
//! cheaper one declines. This module is invoked from
//! [`crate::conflict_watch::on_conflict_detected`] *before*
//! `maybe_spawn_conflict_revision`, so a mechanical resolution retires the
//! conflict with no agent and zero tokens, and only genuinely-semantic
//! conflicts reach the worker path (rung 2 or 3).
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
//! - **Rung 2 (T6) — small focused resolution agent.** When rung 1 leaves a
//!   *bounded* residue of conflicted files ([`rung2_eligible`],
//!   [`RUNG2_MAX_RESIDUAL_FILES`]), [`crate::conflict_watch`] spawns the
//!   revision through the small-agent profile instead of the default one:
//!   `effort_level = small` (the existing effort-level → model table already
//!   resolves `small` to the cheap/fast tier — see `effort.rs` and issue
//!   #746 for why this deliberately never falls to Haiku), and this module
//!   stamps `resolved_by_rung = 2` on the attempt up front (before the
//!   revision has actually run) so the later default-to-rung-3 stamp is a
//!   no-op `COALESCE`. The prompt is the same tight, diagnosis-inline
//!   conflict-resolution fragment rung 3 already uses
//!   (`compose_conflict_resolution_fragment` in `runner.rs`) — it is
//!   already scoped to "resolve only these conflicted hunks," which is what
//!   makes it suitable for a smaller/cheaper model. A residue above the
//!   bound is treated as a large/architectural conflict and declines rung 2
//!   up front, climbing straight to rung 3 (per the design's rung-3 decline
//!   condition).
//!
//!   Deferred from this task: literally handing the *already-leased,
//!   already-rebased* workspace to the rung-2 execution (so the agent never
//!   pays a second lease/goto/rebase) is NOT wired — the coordinator's
//!   dispatch invariant unconditionally re-runs `cube workspace goto` for
//!   every `revision_implementation` dispatch (`coordinator.rs`, "positioning
//!   is never skipped for revisions"), so a second lease+goto+rebase still
//!   happens for rung 2 today. Skipping that safely needs a new explicit
//!   "already positioned" signal threaded through that invariant, which is
//!   a deliberate dispatch-safety change deserving its own review rather
//!   than a silent bypass here. Rung 2 as implemented still gets the
//!   cost/safety win of a small, cheap, bounded-scope agent — it just
//!   doesn't yet skip the re-lease.
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
/// `conflict_resolutions.resolved_by_rung` for telemetry (T1). Rung 0
/// (deterministic resolvers) and rung 1 (engine-direct rebase) are produced
/// by this harness; rung 2 (T6, the small focused resolution agent) is
/// stamped by [`rung2_eligible`]'s caller before it spawns the revision;
/// rung 3 (full worker) is stamped by the existing retire paths.
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

/// Rung 2 (T6): the escalation-ladder rung a resolution was produced at when
/// a small, focused, pre-staged agent — not a cold full worker — resolved
/// it. Recorded in `conflict_resolutions.resolved_by_rung` by
/// [`crate::conflict_watch`] via [`crate::work::WorkDb::stamp_conflict_resolution_rung`]
/// at spawn time, before the revision has actually run, so the later
/// default-to-rung-3 stamp (`mark_conflict_resolution_succeeded`) is a
/// no-op `COALESCE`.
pub(crate) const RUNG_SMALL_RESOLUTION_AGENT: i64 = 2;

/// Upper bound on residual conflicted files rung 2 will accept. Above this,
/// the conflict is treated as large/architectural (design's rung-3 decline
/// condition) and climbs straight to the full worker. Conservative default
/// (single-file only) pending the open design question ("should rung 2 be
/// capped to single-file semantic conflicts in v1?",
/// `merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.attentions.json`)
/// — easy to raise once the small-agent profile has telemetry behind it.
pub(crate) const RUNG2_MAX_RESIDUAL_FILES: usize = 1;
/// Result of running the mechanical rungs against a fresh conflict attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LadderOutcome {
    /// A mechanical rung produced a resolution: the attempt is retired, the
    /// parent is back in Review, and the success events are published. The
    /// caller must NOT spawn a worker revision.
    Retired,
    /// No mechanical rung produced a resolution (rung 1 unavailable, the
    /// rebase errored, or residual conflicts remain). The caller falls
    /// through to the existing worker-spawn path. Any leased workspace has
    /// already been released.
    ///
    /// `residual_conflict_files` is the number of files rung 1's rebase
    /// left conflicted, when known — `None` when the harness never got far
    /// enough to run the rebase (ensure_repo/lease/goto/rebase-transport
    /// failures) or the rebase was clean-but-unpushed. The caller passes
    /// this to [`rung2_eligible`] to decide whether the next worker spawn
    /// should use rung 2's small-agent profile or climb straight to rung 3.
    FellThrough { residual_conflict_files: Option<usize> },
}

/// Rung 2 (T6) eligibility: a bounded set of residual conflicted files is
/// "genuine semantic overlap" a small focused agent may attempt; zero files
/// means rung 1 never actually left a residue to hand off (a lease/goto/
/// rebase-transport failure, or a clean-but-unpushed rebase) and more than
/// [`RUNG2_MAX_RESIDUAL_FILES`] is treated as a large/architectural conflict
/// that declines rung 2 up front, per the design's rung-3 decline condition.
pub(crate) fn rung2_eligible(residual_conflict_files: Option<usize>) -> bool {
    matches!(residual_conflict_files, Some(n) if n > 0 && n <= RUNG2_MAX_RESIDUAL_FILES)
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
        return LadderOutcome::FellThrough {
            residual_conflict_files: None,
        };
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
            return LadderOutcome::FellThrough {
                residual_conflict_files: None,
            };
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "conflict_ladder: failed to resolve repo_remote_url; skipping rung 1",
            );
            return LadderOutcome::FellThrough {
                residual_conflict_files: None,
            };
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
            return LadderOutcome::FellThrough {
                residual_conflict_files: None,
            };
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
            return LadderOutcome::FellThrough {
                residual_conflict_files: None,
            };
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
        return LadderOutcome::FellThrough {
            residual_conflict_files: None,
        };
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
            return LadderOutcome::FellThrough {
                residual_conflict_files: None,
            };
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
        return LadderOutcome::FellThrough {
            residual_conflict_files: None,
        };
    }

    // Residual conflicts after the structural rebase are genuine overlap.
    // Try rung 0 (deterministic resolvers) first — gated off live by
    // RUNG0_APPLY_LIVE (see its doc comment) until T2562's result-gate
    // lands; `attempt_rung0` itself decides whether every residual file
    // actually resolves. If rung 0 is gated off or declines, the caller
    // uses the residual file count to decide between rung 2 (a small
    // focused agent, when the residue is bounded — see `rung2_eligible`)
    // and rung 3 (full worker, for a large/architectural conflict).
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

    let residual_conflict_files = rebase.conflicted_files.len();
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr = pr_number,
        attempt_id = %attempt.id,
        residual_conflicts = residual_conflict_files,
        "conflict_ladder: rung 1 left residual conflicts; falling through to rung 2/3",
    );
    LadderOutcome::FellThrough {
        residual_conflict_files: Some(residual_conflict_files),
    }
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
        return LadderOutcome::FellThrough {
            residual_conflict_files: None,
        };
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
            return LadderOutcome::FellThrough {
                residual_conflict_files: None,
            };
        }
    };

    if let Err(err) = cube_client.push_resolution(&lease.workspace_path, pr_number).await {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr = pr_number,
            error = %format!("{err:#}"),
            "conflict_ladder: rung 0 resolved every residual file but the push failed; climbing to worker",
        );
        return LadderOutcome::FellThrough {
            residual_conflict_files: None,
        };
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
