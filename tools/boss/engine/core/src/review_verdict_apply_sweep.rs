//! Periodic reconciler for staged `review_verdict` proposals.
//!
//! Submission leaves a verdict `proposed` so GitHub probes and remediation
//! task creation do not block the worker socket. This sweep (and the
//! post-submit kick in `app::proposals`) applies those proposals: one
//! verdict/cycle update per batch, a revision while the origin PR is open,
//! or a follow-up against `main` if it has merged.

use std::sync::Arc;
use std::time::Duration;

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::DispatchEventSink;
use crate::work::{GhPrStateChecker, PrInspect, PrOpenState, PrStateChecker, ReviewVerdictApplyStats, WorkDb};

/// Production [`PrStateChecker`] for applying review verdicts.
///
/// Live PR state comes from [`GhPrStateChecker`]. The merged-parent deletion
/// observation spawns `gh` for a GitHub compare, so it is supplied here, above
/// storage, and the applier only receives the owned list of removed surfaces.
/// Every caller that applies a verdict in production must pass this checker
/// (not [`GhPrStateChecker`], whose tripwire is the empty default) or the
/// both-parents deletion halt is silently skipped.
pub(crate) struct GhReviewVerdictChecker;

impl PrStateChecker for GhReviewVerdictChecker {
    fn check(&self, pr_url: &str) -> anyhow::Result<PrOpenState> {
        GhPrStateChecker.check(pr_url)
    }

    fn inspect(&self, pr_url: &str) -> anyhow::Result<PrInspect> {
        GhPrStateChecker.inspect(pr_url)
    }

    fn merged_parent_deletions(
        &self,
        repo_slug: &str,
        head_before: &str,
        base_sha: &str,
        head_after: &str,
    ) -> Vec<String> {
        crate::merge_parent_deletion::compute_merged_parent_deletions_blocking(
            repo_slug,
            head_before,
            base_sha,
            head_after,
        )
    }
}

/// Cadence for the crash-recovery pass. Fresh submissions are applied
/// immediately from the proposal handler; this interval only covers
/// engine restarts and a failed first attempt.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

impl crate::sweep_loop::SweepOutcome for ReviewVerdictApplyStats {
    fn has_activity(&self) -> bool {
        self.applied > 0 || self.failed > 0 || self.superseded > 0
    }

    fn log(&self) {
        tracing::info!(
            applied = self.applied,
            failed = self.failed,
            created_work = self.created_work,
            superseded = self.superseded,
            "review-verdict apply sweep: applied staged review_verdict proposals",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_work_sweep_loop(
        work_db,
        coordinator,
        dispatch_events,
        interval,
        |work_db, coordinator, _dispatch_events| Box::pin(run_one_pass(work_db, coordinator)),
    )
}

/// Apply every still-proposed `review_verdict`. Kicks the scheduler when
/// a revision or follow-up was created so it does not wait for the next
/// heartbeat.
pub async fn run_one_pass(work_db: &WorkDb, coordinator: Arc<ExecutionCoordinator>) -> ReviewVerdictApplyStats {
    let work_db_clone = work_db.clone();
    let stats =
        match tokio::task::spawn_blocking(move || work_db_clone.apply_pending_review_verdicts(&GhReviewVerdictChecker))
            .await
        {
            Ok(Ok(stats)) => stats,
            Ok(Err(error)) => {
                tracing::warn!(
                    ?error,
                    "review-verdict apply sweep: apply query failed; skipping this pass"
                );
                ReviewVerdictApplyStats::default()
            }
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "review-verdict apply sweep: apply task joined with error; skipping this pass"
                );
                ReviewVerdictApplyStats::default()
            }
        };
    if stats.created_work > 0 {
        coordinator.kick();
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verdict checker's tripwire is the engine's compare-based
    /// observation, which short-circuits (no `gh` call) when any ref is
    /// missing — the same fail-open behaviour the completion path has.
    #[test]
    fn the_verdict_checker_observes_merged_parent_deletions_through_the_compare_tripwire() {
        assert!(
            GhReviewVerdictChecker
                .merged_parent_deletions("org/repo", "", "base", "head")
                .is_empty()
        );
        assert!(
            GhReviewVerdictChecker
                .merged_parent_deletions("org/repo", "before", "base", "")
                .is_empty()
        );
    }

    /// The plain PR-state checker never raises the deletion halt: its tripwire
    /// is the trait default. Production verdict application therefore has to
    /// use [`GhReviewVerdictChecker`], which is what both apply sites pass.
    #[test]
    fn the_plain_pr_state_checker_leaves_the_deletion_tripwire_at_its_empty_default() {
        assert!(
            GhPrStateChecker
                .merged_parent_deletions("org/repo", "before", "base", "head")
                .is_empty()
        );
    }
}
