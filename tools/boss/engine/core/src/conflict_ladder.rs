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
//!   3 exactly as an all-declined rung 1 does today. Live by default — see
//!   [`RUNG0_APPLY_LIVE`]'s doc comment for the incident that triggered
//!   flipping it and how to kill it again if needed.
//!
//! ## T9 (T2562) result-gate: the deletion tripwire on rungs 0/1
//!
//! Before either mechanical rung auto-retires a pushed resolution, the
//! harness now calls [`CubeClient::verify_deletion_tripwire`] — the same
//! both-parents deletion tripwire (incident-002 P2,
//! `merge_parent_deletion::compute_merged_parent_deletions`) the
//! worker-driven `pr_review` pass already runs for rung 2/3's output
//! (`completion.rs::compute_merge_parent_deletion_signoff`). A finding
//! halts the attempt in `blocked: deletion_signoff` — the identical
//! operator-sign-off state rung 2/3 land in on the same tripwire — via
//! [`halt_attempt_for_deletion_signoff`], instead of auto-retiring an
//! unvetted deletion. This closes the gap [`RUNG0_APPLY_LIVE`]'s doc
//! comment describes: there is now a standalone check this harness calls
//! to vet a mechanical rung's output before auto-retiring it. Flipping
//! `RUNG0_APPLY_LIVE` itself remains a separate, deliberate follow-up
//! decision (see that constant's doc comment) — this task only wires the
//! gate, it does not turn rung 0 on.
//!
//! The "build gate" half of T9's scope is the PR's own CI: once a rung
//! retires an attempt, the task returns to ordinary `in_review` (or, on a
//! tripwire hit, `blocked: deletion_signoff`) — either way it is a normal
//! task the existing `ci_watch` detection sweep already covers generically
//! for *any* in-review PR, independent of which rung produced the push.
//! No rung-specific build-gate machinery is needed to get that coverage;
//! it falls out of retiring into the same lifecycle every other PR uses.
//!
//! ## Deferred (declared in the T4 PR, still open)
//!
//! - **Escalation-on-rejection → rung 3 with findings, for CI/AI-review
//!   rejections specifically.** The design's aspirational text describes
//!   *any* post-resolution rejection (tripwire, build gate, AI reviewer)
//!   auto-escalating to a rung-3 worker. For the deletion tripwire this
//!   task deliberately does NOT build that: the already-landed P1/P2
//!   behavior for rung 2/3 (`completion.rs`) requires **explicit operator
//!   sign-off** on a flagged deletion rather than any auto-remediation —
//!   incident-002's own lesson is that letting an agent "fix" a flagged
//!   deletion is the failure mode, not the safety net. Extending rungs 0/1
//!   to the *same* human-gated halt (this task) is the correct application
//!   of "compose with T2253, never weaken it": auto-escalating to a rung-3
//!   agent instead would be *weaker* than what rung 2/3 already do. A CI
//!   failure or AI-review severity-gate rejection on a rung 2/3 PR already
//!   mints a normal revision (`ci_watch.rs` / `completion.rs`) — untouched
//!   by this task. What the harness *does* guarantee is the ladder's other
//!   invariant: a rung that **declines** (rebase errors, an unresolvable
//!   residual file, or a failed push) climbs — it never retries the same
//!   rung against the same state (the `conflict_resolutions` UNIQUE key
//!   dedupes identical base+head states; a genuinely new state gets a
//!   fresh attempt, bounded by the churn guard).

use boss_deterministic_resolvers::{ConflictedFile, RegistryResolution, ResolvedFile, ResolverRegistry};
use boss_protocol::{CreateAttentionItemInput, FrontendEvent};

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
/// fully implemented and unit-tested. T2562 (T9, "T2253 safety integration
/// for the ladder") landed the result-gate: both rungs 0 and 1 route their
/// pushed output through [`CubeClient::verify_deletion_tripwire`] before
/// auto-retiring (see the module doc comment) — the safety net this
/// constant was waiting on.
///
/// Flipped live by the reviewed follow-up PR that constant's prior doc
/// comment called for, triggered by spinyfin/mono#2032 (chore T2680):
/// with rung 0 gated off, a PR whose *re*-conflict was lockfile-only
/// (`MODULE.bazel.lock` regenerated from a `bzlmod` dependency bump on
/// `main`, "no source files required resolution this round" per the
/// resolving revision's own PR comment) still spawned a full LLM
/// "resolve merge conflict" revision instead of retiring for free —
/// exactly the recurring case rung 0 exists to absorb cheaply. See
/// `rung0_live_resolves_a_lockfile_only_residue_including_on_a_second_conflict`
/// in `conflict_ladder_tests.rs`.
///
/// Deliberately still a compile-time constant, not a `feature_flags`
/// debug-pane toggle: kept as a one-line kill switch (set back to `false`)
/// in case live rung-0 activity misbehaves, without touching runtime
/// config.
const RUNG0_APPLY_LIVE: bool = true;

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
    /// A mechanical rung (0 or 1) pushed a resolution, but the T9/T2562
    /// both-parents deletion tripwire rejected it
    /// ([`halt_attempt_for_deletion_signoff`]): the attempt is halted in
    /// `blocked: deletion_signoff` pending operator sign-off — the same
    /// state rung 2/3's worker-driven resolutions land in on the same
    /// tripwire — rather than retired as a success. Like `Retired`, the
    /// caller must NOT spawn a worker revision (there is no automatic
    /// remediation for a flagged deletion), but callers must not log or
    /// report this as an auto-resolution.
    HaltedForSignoff,
    /// No mechanical rung produced a resolution (rung 1 unavailable, the
    /// rebase errored, or residual conflicts remain). The caller falls
    /// through to the existing worker-spawn path. Any leased workspace has
    /// already been released.
    ///
    /// `residual_conflict_files` is the number of files rung 1's rebase
    /// left conflicted, when known — `None` when the harness never got far
    /// enough to run the rebase (ensure_repo/goto/rebase-transport
    /// failures) or the rebase was clean-but-unpushed. The caller passes
    /// this to [`rung2_eligible`] to decide whether the next worker spawn
    /// should use rung 2's small-agent profile or climb straight to rung 3.
    FellThrough { residual_conflict_files: Option<usize> },
    /// Rung 1 could not even be attempted this pass: leasing a workspace
    /// failed twice in a row (the initial attempt and one retry — see
    /// [`lease_rung1_workspace`]). This is deliberately distinct from
    /// [`Self::FellThrough`]: a lease failure is an infrastructure hiccup
    /// (the incident that motivated this variant was a transient cube
    /// dirty-reclaim refusal — `LeaseExpiredWorkspaceDirty`, cube exit code
    /// 7 — that a retry 3 seconds later cleared), not evidence that the
    /// conflict is large/semantic. The caller must NOT spawn a worker
    /// revision on this signal — escalating straight to the most expensive
    /// rung on a pure infra failure is exactly the bug this variant exists
    /// to close. The attempt stays `pending` with no `revision_task_id`, so
    /// the ladder is retried in full on the next `conflict_watch` tick.
    MechanicalRungsUnavailable,
}

/// Emits the single canonical decision line for a conflicted-PR
/// evaluation, so "did the deterministic path fire, and why (not)?" is
/// always answerable from the engine trace by grepping this one event
/// name — instead of reconstructing it from scattered debug-level branch
/// logging or, worse, finding nothing at all (mono#1398/#1764,
/// spinyfin/mono#2032: this decision has previously been silent).
///
/// `verdict` is one of:
/// - `"deterministic"` — a mechanical rung (0 or 1) produced or halted a
///   resolution; no worker revision was spawned.
/// - `"generic"` — no mechanical rung applied; a full (or small-agent,
///   rung 2) worker revision will be spawned.
/// - `"skip"` — the ladder was never attempted at all (e.g. the
///   `conflict_ladder_mechanical_rebase` feature flag is off).
pub(crate) fn log_routing_verdict(
    work_item_id: &str,
    pr: Option<u64>,
    conflicted_files: &[String],
    verdict: &str,
    reason: &str,
) {
    tracing::info!(
        work_item_id,
        pr,
        conflicted_files = ?conflicted_files,
        verdict,
        reason,
        "conflict_ladder: routing verdict",
    );
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
        log_routing_verdict(
            &candidate.work_item_id,
            None,
            &[],
            "generic",
            "could not parse a PR number from pr_url; mechanical rungs skipped",
        );
        return LadderOutcome::FellThrough {
            residual_conflict_files: None,
        };
    };
    let pr_number = pr_number as u64;

    // One INFO line at ladder entry so "was any mechanical rung invoked for
    // this conflict?" is answerable from the engine trace (mono#1398/#1764:
    // the ladder was running silently / never reached). Escalation and
    // outcome lines follow from `run_rung1_in_lease` / `attempt_rung0`.
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr = pr_number,
        attempt_id = %attempt.id,
        "conflict_ladder: entering mechanical rungs (rung 1 engine-direct rebase)",
    );

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
            log_routing_verdict(
                &candidate.work_item_id,
                Some(pr_number),
                &[],
                "generic",
                "no repo_remote_url resolves for this work item; mechanical rungs skipped",
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
            log_routing_verdict(
                &candidate.work_item_id,
                Some(pr_number),
                &[],
                "generic",
                "failed to resolve repo_remote_url; mechanical rungs skipped",
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
            log_routing_verdict(
                &candidate.work_item_id,
                Some(pr_number),
                &[],
                "generic",
                "ensure_repo failed; mechanical rungs skipped",
            );
            return LadderOutcome::FellThrough {
                residual_conflict_files: None,
            };
        }
    };

    let task_label = format!("conflict-ladder rung1 {}", candidate.work_item_id);
    let lease = match lease_rung1_workspace(cube_client, &repo.repo_id, &task_label).await {
        Ok(lease) => lease,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                repo_id = %repo.repo_id,
                error = %format!("{err:#}"),
                "conflict_ladder: could not lease a workspace for rung 1 after retry; mechanical rungs \
                 unavailable this attempt",
            );
            log_routing_verdict(
                &candidate.work_item_id,
                Some(pr_number),
                &[],
                "skip",
                "could not lease a workspace for rung 1 after one retry; mechanical rungs unavailable \
                 this attempt, ladder will retry on the next conflict_watch tick",
            );
            return LadderOutcome::MechanicalRungsUnavailable;
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

/// Lease a rung-1 workspace, retrying once on any error.
///
/// The lease step is the one mechanical-rung failure mode known in
/// practice to be transient: `cube workspace lease` reclaiming an expired
/// lease refuses to touch a dirty working copy
/// (`LeaseExpiredWorkspaceDirty`, cube exit code 7) until cube's own
/// expiry sweep has cleared it — and that sweep runs as a side effect of
/// the *first* lease attempt itself, so an immediate second attempt lands
/// on a clean path. The 2026-07-16 incident that motivated this retry saw
/// exactly that: a full agent's own lease of the same workspace succeeded
/// 3 seconds after the ladder's first (and only) attempt had failed.
///
/// Deliberately one retry, not a loop: if the second attempt also fails,
/// this is either a genuinely-down cube or a stuck workspace, and further
/// retries here would only delay the ladder without helping — the caller
/// treats a persistent failure as [`LadderOutcome::MechanicalRungsUnavailable`]
/// so the next `conflict_watch` tick tries again from scratch instead of
/// escalating straight to a worker.
async fn lease_rung1_workspace(
    cube_client: &dyn CubeClient,
    repo_id: &str,
    task_label: &str,
) -> anyhow::Result<CubeWorkspaceLease> {
    match cube_client.lease_workspace(repo_id, task_label, None, false, &[]).await {
        Ok(lease) => Ok(lease),
        Err(first_err) => {
            tracing::debug!(
                repo_id,
                error = %format!("{first_err:#}"),
                "conflict_ladder: rung 1 lease failed; retrying once",
            );
            cube_client
                .lease_workspace(repo_id, task_label, None, false, &[])
                .await
                .map_err(|second_err| second_err.context(format!("first lease attempt also failed: {first_err:#}")))
        }
    }
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
        log_routing_verdict(
            &candidate.work_item_id,
            Some(pr_number),
            &[],
            "generic",
            "goto_workspace failed; mechanical rungs unavailable",
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
            log_routing_verdict(
                &candidate.work_item_id,
                Some(pr_number),
                &[],
                "generic",
                "engine-direct rebase (rung 1) failed; mechanical rungs unavailable",
            );
            return LadderOutcome::FellThrough {
                residual_conflict_files: None,
            };
        }
    };

    if rebase.clean {
        if rebase.pushed {
            // T9 (T2562): vet rung 1's pushed resolution against the
            // both-parents deletion tripwire before auto-retiring.
            let deletions = verify_deletion_tripwire(work_db, cube_client, candidate, attempt, pr_number).await;
            if !deletions.is_empty() {
                halt_attempt_for_deletion_signoff(
                    work_db,
                    publisher,
                    candidate,
                    attempt,
                    RUNG_ENGINE_DIRECT_REBASE,
                    &[],
                    &deletions,
                )
                .await;
                return LadderOutcome::HaltedForSignoff;
            }
            // Rung 1 rebased cleanly and pushed the updated branch — the PR is
            // resolved with no agent. Retire the attempt at rung 1.
            retire_attempt_at_rung(work_db, publisher, candidate, attempt, RUNG_ENGINE_DIRECT_REBASE, &[]).await;
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
        log_routing_verdict(
            &candidate.work_item_id,
            Some(pr_number),
            &[],
            "generic",
            "rung 1 rebased clean but the push was skipped upstream; mechanical resolution not landed",
        );
        return LadderOutcome::FellThrough {
            residual_conflict_files: None,
        };
    }

    // Residual conflicts after the structural rebase are genuine overlap.
    // Try rung 0 (deterministic resolvers) first — gated off live by
    // RUNG0_APPLY_LIVE (see its doc comment). `attempt_rung0` itself
    // decides whether every residual file actually resolves, and (T9/T2562)
    // vets any pushed result against the deletion tripwire before retiring.
    // If rung 0 is gated off, declines, or its resolution is halted for
    // sign-off, the caller uses the residual file count to decide between
    // rung 2 (a small focused agent, when the residue is bounded — see
    // `rung2_eligible`) and rung 3 (full worker, for a large/architectural
    // conflict).
    if RUNG0_APPLY_LIVE {
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            pr = pr_number,
            attempt_id = %attempt.id,
            residual_conflicts = rebase.conflicted_files.len(),
            "conflict_ladder: escalating to rung 0 (deterministic resolvers) for rung-1 residue",
        );
        let rung0_outcome = attempt_rung0(
            work_db,
            publisher,
            cube_client,
            candidate,
            attempt,
            lease,
            &rebase.conflicted_files,
        )
        .await;
        if !matches!(rung0_outcome, LadderOutcome::FellThrough { .. }) {
            return rung0_outcome;
        }
    } else {
        // Rung 0 is compile-time gated off (see `RUNG0_APPLY_LIVE`). Emit one
        // line so the engine trace records *why* the deterministic resolvers
        // show zero activity — a config fact, not a dispatch/logging bug
        // (mono#1398/#1764).
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            pr = pr_number,
            attempt_id = %attempt.id,
            residual_conflicts = rebase.conflicted_files.len(),
            rung0_apply_live = RUNG0_APPLY_LIVE,
            "conflict_ladder: rung 0 (deterministic resolvers) gated off; not attempted — climbing to rung 2/3",
        );
    }

    let residual_conflict_files = rebase.conflicted_files.len();
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr = pr_number,
        attempt_id = %attempt.id,
        residual_conflicts = residual_conflict_files,
        "conflict_ladder: rung 1 left residual conflicts; falling through to rung 2/3",
    );
    log_routing_verdict(
        &candidate.work_item_id,
        Some(pr_number),
        &rebase.conflicted_files,
        "generic",
        if RUNG0_APPLY_LIVE {
            "rung 0 did not resolve every residual file; falling through to rung 2/3"
        } else {
            "rung 0 compile-gated off (RUNG0_APPLY_LIVE=false); falling through to rung 2/3"
        },
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

    // Rung 1's `conflicted_files` are bare paths — `coordinator::parse_rebase_payload`
    // strips jj's trailing conflict-type descriptor (e.g. "    2-sided conflict")
    // before these ever reach here. They are not the merge-tree-derived diagnosis
    // `boss_conflict_diagnosis` produces elsewhere — there is no marker-tree
    // available here, so `shape` is the same generic "content" the
    // producer-rebase diagnosis path uses for the same reason (see `conflict_res.rs`).
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
            // Distinguish "no registered resolver applies to this file" from
            // "a resolver matched and ran, then itself declined" — collapsing
            // both into one generic message previously sent diagnosis down
            // the wrong path (mono#2067 reconciliation): a resolver-matched
            // decline means rung 0's registry logic needs attention, while a
            // no-resolver-applies file means the residue is simply out of
            // rung 0's coverage. Per-file reasons make either case
            // answerable straight from this trace line.
            let per_file: Vec<String> = declined
                .iter()
                .map(|d| {
                    if d.matched_resolver {
                        format!("{}: resolver ran and declined: {}", d.path, d.reason)
                    } else {
                        format!("{}: no resolver applies to this file", d.path)
                    }
                })
                .collect();
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr = pr_number,
                resolved = resolved.len(),
                declined = declined.len(),
                declined_files = ?per_file,
                "conflict_ladder: rung 0 declined; climbing to worker",
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

    // T9 (T2562): vet rung 0's pushed resolution against the both-parents
    // deletion tripwire before auto-retiring.
    let deletions = verify_deletion_tripwire(work_db, cube_client, candidate, attempt, pr_number).await;
    if !deletions.is_empty() {
        halt_attempt_for_deletion_signoff(
            work_db,
            publisher,
            candidate,
            attempt,
            RUNG_DETERMINISTIC_RESOLVER,
            residual_paths,
            &deletions,
        )
        .await;
        return LadderOutcome::HaltedForSignoff;
    }

    retire_attempt_at_rung(
        work_db,
        publisher,
        candidate,
        attempt,
        RUNG_DETERMINISTIC_RESOLVER,
        residual_paths,
    )
    .await;
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
    conflicted_files: &[String],
) {
    log_routing_verdict(
        &candidate.work_item_id,
        parse_pr_number(&candidate.pr_url).map(|n| n as u64),
        conflicted_files,
        "deterministic",
        &format!("mechanical rung {rung} resolved every conflicted file; no worker spawned"),
    );

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

/// T9 (T2562) result-gate: verify a mechanical rung's (0 or 1) freshly
/// pushed resolution against the both-parents deletion tripwire
/// (incident-002 P2) before the caller retires the attempt. Delegates to
/// [`CubeClient::verify_deletion_tripwire`] (test doubles fail open with
/// no network call by default; only [`crate::coordinator::CommandCubeClient`]
/// makes the real gh-backed check).
///
/// Re-derives the repo slug from `work_db` rather than adding a parameter
/// to every call site — mirrors [`attempt_rung0`]'s existing
/// re-derive-rather-than-thread pattern for `pr_number`. Returns an empty
/// set (fails open) when `head_sha_before` / `base_sha_at_trigger` are
/// unrecorded on the attempt or the repo slug is unresolvable.
///
/// **Fail-open has no backstop on these rungs.** The worker-driven rung
/// 2/3 path is double-checked: even if this tripwire itself failed open,
/// the spawned `pr_review` pass re-derives the same
/// `compute_merged_parent_deletions` check on the worker's actual PR
/// output before it can retire. Rungs 0 and 1 retire straight to
/// `in_review` with **no worker spawned at all**, so a transient
/// `gh`/network error here (in [`crate::coordinator::CommandCubeClient`]'s
/// `fetch_pr_head_sha` or the `gh compare` calls inside
/// `compute_merged_parent_deletions`) — as opposed to a genuinely clean
/// tripwire result — silently auto-retires a possibly-deletion-bearing
/// mechanical resolution with nothing left to catch it. This is a known,
/// currently-accepted gap (see the module doc's "Deferred" section), not
/// an oversight.
async fn verify_deletion_tripwire(
    work_db: &WorkDb,
    cube_client: &dyn CubeClient,
    candidate: &PendingMergeCheck,
    attempt: &ConflictResolution,
    pr_number: u64,
) -> Vec<String> {
    let (Some(head_before), Some(base_sha)) = (
        attempt.head_sha_before.as_deref(),
        attempt.base_sha_at_trigger.as_deref(),
    ) else {
        return Vec::new();
    };
    let Some(repo_slug) = work_db
        .resolve_repo_for_task(&candidate.work_item_id)
        .ok()
        .flatten()
        .and_then(|remote| crate::completion::parse_repo_slug(&remote).ok())
    else {
        return Vec::new();
    };
    cube_client
        .verify_deletion_tripwire(&repo_slug, head_before, base_sha, pr_number)
        .await
}

/// Halt a `conflict_resolutions` attempt a mechanical rung (0 or 1) pushed
/// but the T9/T2562 deletion tripwire rejected: stamp the attempt
/// `succeeded` at `rung` (it did produce a mechanical resolution — the
/// telemetry fact stands independent of the safety gate), flip the parent
/// from `blocked: merge_conflict` to `blocked: deletion_signoff` via
/// [`WorkDb::mark_chore_blocked_deletion_signoff`], and file the same
/// operator sign-off attention item `completion.rs`'s worker-driven
/// `pr_review` path files for rung 2/3 — so a deletion halts identically
/// regardless of which rung produced it. Does NOT publish
/// `ConflictResolutionSucceeded` (this is not a success) and does not spawn
/// any worker (there is no automatic remediation for a flagged deletion —
/// see the module doc comment's "Deferred" section for why).
async fn halt_attempt_for_deletion_signoff(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    attempt: &ConflictResolution,
    rung: i64,
    conflicted_files: &[String],
    deletions: &[String],
) {
    log_routing_verdict(
        &candidate.work_item_id,
        parse_pr_number(&candidate.pr_url).map(|n| n as u64),
        conflicted_files,
        "deterministic",
        &format!(
            "mechanical rung {rung} resolved every conflicted file but the deletion tripwire halted it pending sign-off"
        ),
    );

    if let Err(err) = work_db.mark_conflict_resolution_succeeded_at_rung(&attempt.id, None, rung) {
        tracing::warn!(
            attempt_id = %attempt.id,
            rung,
            ?err,
            "conflict_ladder: failed to stamp attempt on deletion-signoff halt",
        );
    }

    match work_db.mark_chore_blocked_deletion_signoff(&candidate.work_item_id, &candidate.pr_url) {
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                rung,
                "conflict_ladder: could not flip task to blocked:deletion_signoff (already moved?)",
            );
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                rung,
                ?err,
                "conflict_ladder: failed to flip task to blocked:deletion_signoff",
            );
        }
    }

    let _ = work_db.create_attention_item(CreateAttentionItemInput {
        work_item_id: Some(candidate.work_item_id.clone()),
        kind: crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND.to_owned(),
        title: crate::merge_parent_deletion::SIGNOFF_ATTENTION_TITLE.to_owned(),
        body_markdown: crate::merge_parent_deletion::render_signoff_attention_body(deletions, &candidate.pr_url),
        execution_id: None,
        status: None,
        resolved_at: None,
    });

    // Clear the in-flight merge_conflict signal so `maybe_clear_blocked` does
    // not re-fire on the next probe — mirrors `retire_attempt_at_rung`.
    if let Err(err) = work_db.clear_merge_conflict_signal_only(&candidate.work_item_id) {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            rung,
            ?err,
            "conflict_ladder: failed to clear in-flight signal on deletion-signoff halt",
        );
    }

    publisher
        .publish_work_item_changed(
            &candidate.product_id,
            &candidate.work_item_id,
            "pr_review_deletion_signoff",
        )
        .await;

    tracing::warn!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        rung,
        removed = deletions.len(),
        "conflict_ladder: merge-parent deletion tripwire fired for a mechanical rung; halted in \
         blocked:deletion_signoff pending operator sign-off (T9/T2562)",
    );
}

#[cfg(test)]
#[path = "conflict_ladder_tests.rs"]
mod tests;
