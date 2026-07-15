//! Layer 4 speculative conflict prediction (T10, `future / not a v1
//! blocker` in
//! `docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`).
//!
//! Piggybacks on the merge poller's periodic sweep: for each in-review PR
//! whose recheck interval has elapsed, lease a scratch workspace and run a
//! throwaway, no-push engine-direct rebase (`cube workspace rebase --pr <n>
//! --no-push`) against current `main`. No worker is spawned and the real PR
//! branch is never mutated — [`crate::coordinator::CubeClient::rebase_workspace_no_push`]
//! is the same engine-direct mechanical rebase rung 1 uses, minus the push.
//! Outcomes:
//!
//!   - **Clean** — nothing to do; only a counter is incremented (the
//!     design's "record the negative").
//!   - **Conflict predicted** — recorded to `conflict_resolutions`
//!     telemetry (`event_source = 'speculative_predicted'`) *before* the PR
//!     would otherwise reach `conflict_watch`, improving the hotspot
//!     signal early.
//!
//! Deliberately out of scope here (see this task's PR description):
//! pre-emptively running rung 0/1 resolution against a predicted conflict,
//! and converting two mutually-predicted-to-conflict in-flight branches
//! into an ordered stack (T11) — the design marks both as separate,
//! optional follow-on work.
//!
//! Gated by the `speculative_conflict_prediction` feature flag (default
//! OFF, see [`crate::completion::WorkerCompletionHandler::speculative_conflict_prediction_enabled`])
//! and rate-limited two ways to bound workspace-lease churn: a
//! per-work-item recheck interval ([`SpeculativeCheckSchedule`]) and a
//! per-sweep cap ([`MAX_CHECKS_PER_PASS`]) on how many candidates are
//! checked at all.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::coordinator::CubeClient;
use crate::merge_poller::{parse_pr_number, sanitize_metric_name_component};
use crate::metrics::Registry;
use crate::work::{PendingMergeCheck, SpeculativeConflictInsertInput, WorkDb};

/// Minimum time between speculative rebase attempts for the same work
/// item — the design's "must be rate-limited to avoid churn".
const RECHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Upper bound on how many speculative rebases one sweep will attempt, so a
/// burst of newly-in-review PRs can't flood cube with leases in a single
/// pass. A large backlog still drains within a few passes since the
/// periodic sweep interval is short relative to [`RECHECK_INTERVAL`].
const MAX_CHECKS_PER_PASS: usize = 3;

crate::register_counter!(
    SPECULATIVE_CONFLICT_PREDICTED,
    "speculative_conflict.predicted",
    "Speculative rebases in one sweep that predicted a conflict against current main."
);
crate::register_counter!(
    SPECULATIVE_CONFLICT_CLEAN,
    "speculative_conflict.clean",
    "Speculative rebases in one sweep that predicted no conflict against current main."
);

/// Register this module's counter handles. Called from
/// [`crate::metrics_init::init_all`] at engine startup.
pub fn init(registry: &Registry) {
    registry.register_counter(&SPECULATIVE_CONFLICT_PREDICTED);
    registry.register_counter(&SPECULATIVE_CONFLICT_CLEAN);
}

/// Increment the per-product speculative-outcome counter. Layer 0's hard
/// requirement ("every counter must be scopable per-product") applies to
/// this sweep's telemetry too — mirrors
/// [`crate::merge_poller::record_conflict_class_counter`].
fn record_outcome_counter(registry: &Registry, product_id: &str, predicted: bool) {
    let name = format!(
        "speculative_conflict.{}.{}",
        sanitize_metric_name_component(product_id),
        if predicted { "predicted" } else { "clean" },
    );
    let description = if predicted {
        "Speculative rebases for this product that predicted a conflict against current main."
    } else {
        "Speculative rebases for this product that predicted no conflict against current main."
    };
    registry.counter_inc_by_dynamic(&name, description, 1);
}

/// In-memory, best-effort rate limiter: the last time each work item's PR
/// was speculatively rechecked. Purely in-memory like
/// [`crate::merge_poller`]'s `PrPollSchedule` — a dropped/forgotten entry
/// after a restart just means the PR is eligible for an immediate recheck,
/// never a correctness problem since this whole path is telemetry-only and
/// never mutates the real PR branch.
#[derive(Default)]
pub struct SpeculativeCheckSchedule {
    last_checked_at: HashMap<String, Instant>,
}

impl SpeculativeCheckSchedule {
    fn due(&self, work_item_id: &str, now: Instant) -> bool {
        match self.last_checked_at.get(work_item_id) {
            Some(&last) => now.duration_since(last) >= RECHECK_INTERVAL,
            None => true,
        }
    }

    fn mark_checked(&mut self, work_item_id: &str, now: Instant) {
        self.last_checked_at.insert(work_item_id.to_owned(), now);
    }

    /// Drop tracked entries for work items no longer among the current
    /// candidate set, so the map doesn't grow unbounded across a
    /// long-running engine process.
    fn retain_known(&mut self, known: &HashSet<String>) {
        self.last_checked_at.retain(|id, _| known.contains(id));
    }
}

/// Piggyback on the merge poller's sweep: for each in-review candidate
/// whose recheck interval has elapsed (capped at [`MAX_CHECKS_PER_PASS`]
/// per pass), run a throwaway, no-push engine-direct rebase against current
/// `main` in a leased scratch workspace and record the outcome to
/// telemetry. Errors along the lease → goto → rebase path are logged and
/// skipped — a transient cube/GitHub failure just means this candidate is
/// retried on its next due recheck.
pub async fn run_speculative_pass(
    work_db: &WorkDb,
    cube_client: &dyn CubeClient,
    metrics: &Registry,
    schedule: &mut SpeculativeCheckSchedule,
    candidates: &[PendingMergeCheck],
) {
    let now = Instant::now();
    let known: HashSet<String> = candidates.iter().map(|c| c.work_item_id.clone()).collect();
    schedule.retain_known(&known);

    let due: Vec<&PendingMergeCheck> = candidates
        .iter()
        .filter(|c| schedule.due(&c.work_item_id, now))
        .take(MAX_CHECKS_PER_PASS)
        .collect();
    if due.is_empty() {
        return;
    }
    tracing::debug!(
        due = due.len(),
        total_candidates = candidates.len(),
        "speculative_conflict: sweep starting",
    );
    for candidate in due {
        schedule.mark_checked(&candidate.work_item_id, now);
        check_one(work_db, cube_client, metrics, candidate).await;
    }
}

async fn check_one(work_db: &WorkDb, cube_client: &dyn CubeClient, metrics: &Registry, candidate: &PendingMergeCheck) {
    let Some(pr_number) = parse_pr_number(&candidate.pr_url).filter(|n| *n > 0) else {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "speculative_conflict: could not parse PR number; skipping",
        );
        return;
    };
    let pr_number = pr_number as u64;

    // Same unified opt-out the escalation ladder and conflict_watch honour
    // (Phase 6 #18 / design Q7): a product that has disabled auto PR
    // maintenance gets no engine-direct workspace activity at all, not even
    // a read-only speculative one. A lookup error falls through to
    // "enabled" so a transient DB blip doesn't silently drop the signal.
    match work_db.product_auto_pr_maintenance_enabled(&candidate.product_id) {
        Ok(false) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                product_id = %candidate.product_id,
                "speculative_conflict: product opted out of auto_pr_maintenance; skipping",
            );
            return;
        }
        Ok(true) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "speculative_conflict: failed to read auto_pr_maintenance_enabled; treating as enabled",
            );
        }
    }

    let repo_remote = match work_db.resolve_repo_for_task(&candidate.work_item_id) {
        Ok(Some(url)) => url,
        Ok(None) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                "speculative_conflict: no repo_remote_url resolves for work item; skipping",
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "speculative_conflict: failed to resolve repo_remote_url; skipping",
            );
            return;
        }
    };

    let repo = match cube_client.ensure_repo(&repo_remote).await {
        Ok(repo) => repo,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                repo_remote = %repo_remote,
                error = %format!("{err:#}"),
                "speculative_conflict: ensure_repo failed; skipping",
            );
            return;
        }
    };

    let task_label = format!("speculative-conflict {}", candidate.work_item_id);
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
                "speculative_conflict: could not lease a workspace; skipping",
            );
            return;
        }
    };

    // From here the lease is held: run the check and release unconditionally.
    run_in_lease(
        work_db,
        cube_client,
        metrics,
        candidate,
        pr_number,
        &lease.workspace_path,
    )
    .await;

    if let Err(err) = cube_client.release_workspace(&lease.lease_id).await {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            lease_id = %lease.lease_id,
            ?err,
            "speculative_conflict: releasing workspace failed (likely already released)",
        );
    }
}

/// Position the leased scratch workspace on the PR head, run the no-push
/// engine-direct rebase, and record the outcome. Split out so the caller
/// can release the lease unconditionally regardless of which branch this
/// returns from.
async fn run_in_lease(
    work_db: &WorkDb,
    cube_client: &dyn CubeClient,
    metrics: &Registry,
    candidate: &PendingMergeCheck,
    pr_number: u64,
    workspace_path: &Path,
) {
    if let Err(err) = cube_client.goto_workspace(workspace_path, pr_number).await {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr = pr_number,
            error = %format!("{err:#}"),
            "speculative_conflict: goto_workspace failed; skipping",
        );
        return;
    }

    let rebase = match cube_client.rebase_workspace_no_push(workspace_path, pr_number).await {
        Ok(outcome) => outcome,
        Err(err) => {
            // Includes the default trait-method error on any `CubeClient`
            // that hasn't implemented the no-push rebase — expected and
            // silent-ish (debug, not warn) for every test double and for
            // hosts that predate this capability.
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr = pr_number,
                error = %format!("{err:#}"),
                "speculative_conflict: no-push rebase unavailable or failed; skipping",
            );
            return;
        }
    };

    if rebase.clean {
        SPECULATIVE_CONFLICT_CLEAN.inc(metrics);
        record_outcome_counter(metrics, &candidate.product_id, false);
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr = pr_number,
            "speculative_conflict: predicted clean against current main",
        );
        return;
    }

    SPECULATIVE_CONFLICT_PREDICTED.inc(metrics);
    record_outcome_counter(metrics, &candidate.product_id, true);
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr = pr_number,
        conflicted_files = rebase.conflicted_files.len(),
        "speculative_conflict: predicted a conflict against current main",
    );
    if let Err(err) = work_db.record_speculative_conflict_prediction(SpeculativeConflictInsertInput {
        product_id: candidate.product_id.clone(),
        work_item_id: candidate.work_item_id.clone(),
        pr_url: candidate.pr_url.clone(),
        pr_number: pr_number as i64,
        conflicted_files: rebase.conflicted_files,
    }) {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            ?err,
            "speculative_conflict: failed to record predicted-conflict telemetry",
        );
    }
}

#[cfg(test)]
#[path = "speculative_conflict_tests.rs"]
mod tests;
