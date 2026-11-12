//! Periodic recheck of items held by a cube positioning refusal.
//!
//! A refusal (closed or merged PR, missing head or branch) fails the execution
//! after one attempt and holds the item so nothing re-dispatches into the same
//! refusal. This sweep is what lowers the hold again without a person: each pass
//! probes the live PR state for every held item and, once cube would accept it,
//! clears the hold and its attention item together and resumes normal dispatch.
//! While the refusal persists the probe is read-only: no execution, lease or
//! attention item is created.
//!
//! Live holds reuse [`PrStateChecker::inspect`] — the same `gh pr view` path as
//! the neighbouring review-recovery sweep — and treat a PR as positionable when
//! that inspect reports [`PrOpenState::Open`] with a full 40-character head SHA
//! and a non-empty head branch name. Cube's `goto --pr` refuses immediately
//! when GitHub omits `head.ref`, before any pull-ref fallback, so this sweep
//! keeps the same check. Items with no PR to probe stay held until an explicit
//! launch.
//!
//! Terminal rows (`done`, `archived`, deleted) are a resolve-only pass: the PR
//! that refused positioning is often the one that just merged, so probing it
//! would never clear the hold. The attention is lowered without minting work.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::RequestExecutionInput;

use crate::coordinator::ExecutionCoordinator;
use crate::work::{PrInspect, PrOpenState, PrStateChecker, WorkDb};

/// Counts from one pass; logged at `info` when a hold was lowered.
#[derive(Debug, Default)]
pub struct PositioningRecheckOutcome {
    pub held: usize,
    pub cleared: usize,
    pub restarted: usize,
    pub probe_errors: usize,
}

impl crate::sweep_loop::SweepOutcome for PositioningRecheckOutcome {
    fn has_activity(&self) -> bool {
        self.cleared > 0
    }

    fn log(&self) {
        tracing::info!(
            held = self.held,
            cleared = self.cleared,
            restarted = self.restarted,
            probe_errors = self.probe_errors,
            "positioning-refusal recheck sweep: pass complete",
        );
    }
}

/// Cube would accept positioning: the PR is still open, GitHub reports a
/// full 40-character head commit, and a non-empty head branch name. Cube's
/// `goto --pr` refuses a missing `head.ref` before any SHA fallback.
/// Open vs merged vs closed-unmerged is the shared
/// [`crate::work::classify_pr_merge_state`] decision inside inspect.
fn pr_is_positionable(inspect: &PrInspect) -> bool {
    matches!(inspect.open_state, PrOpenState::Open)
        && inspect
            .head_sha
            .as_deref()
            .is_some_and(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        && inspect.head_ref_name.as_deref().is_some_and(|name| !name.is_empty())
}

/// Spawn the sweep, firing on spawn and then every `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    checker: Arc<dyn PrStateChecker>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let checker = Arc::clone(&checker);
        async move { run_one_pass(&work_db, &coordinator, &checker).await }
    })
}

/// Run one recheck pass over every held item.
pub async fn run_one_pass(
    work_db: &WorkDb,
    coordinator: &Arc<ExecutionCoordinator>,
    checker: &Arc<dyn PrStateChecker>,
) -> PositioningRecheckOutcome {
    let mut outcome = PositioningRecheckOutcome::default();
    let holds = match work_db.list_positioning_holds() {
        Ok(holds) => holds,
        Err(err) => {
            tracing::warn!(
                ?err,
                "positioning-refusal recheck sweep: failed to list holds; skipping pass"
            );
            return outcome;
        }
    };
    outcome.held = holds.len();
    let claimed = coordinator.all_claimed_execution_ids().await;
    for hold in holds {
        if hold.terminal {
            match work_db.clear_positioning_hold(&hold.work_item_id) {
                Ok(true) => {
                    outcome.cleared += 1;
                    tracing::info!(
                        work_item_id = %hold.work_item_id,
                        "positioning-refusal recheck sweep: terminal hold lowered"
                    );
                }
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(work_item_id = %hold.work_item_id, ?err, "positioning-refusal recheck sweep: failed to lower terminal hold; keeping it");
                }
            }
            continue;
        }
        let Some(pr_url) = hold.pr_url.clone() else {
            continue;
        };
        let checker = Arc::clone(checker);
        let inspect = match tokio::task::spawn_blocking(move || checker.inspect(&pr_url)).await {
            Ok(Ok(inspect)) => inspect,
            Ok(Err(err)) => {
                tracing::warn!(work_item_id = %hold.work_item_id, ?err, "positioning-refusal recheck sweep: probe failed; keeping hold");
                outcome.probe_errors += 1;
                continue;
            }
            Err(err) => {
                tracing::warn!(work_item_id = %hold.work_item_id, ?err, "positioning-refusal recheck sweep: probe task failed; keeping hold");
                outcome.probe_errors += 1;
                continue;
            }
        };
        if !pr_is_positionable(&inspect) {
            continue;
        }
        match work_db.clear_positioning_hold(&hold.work_item_id) {
            Ok(true) => {
                outcome.cleared += 1;
                // A deferred item stays parked: only a human approves it, and the
                // request below would mint an execution for it regardless.
                let deferred = matches!(
                    work_db.get_work_item(&hold.work_item_id),
                    Ok(boss_protocol::WorkItem::Task(ref t) | boss_protocol::WorkItem::Chore(ref t)) if t.deferred
                );
                if hold.restarts && !deferred {
                    // Ordinary request, not an explicit start: automatic recovery
                    // must not reopen a retired spawning attempt.
                    match work_db.request_execution_with_live_check(
                        RequestExecutionInput::builder()
                            .work_item_id(hold.work_item_id.clone())
                            .build(),
                        |id| claimed.contains(id),
                    ) {
                        Ok(_) => outcome.restarted += 1,
                        Err(err) => {
                            tracing::warn!(
                                work_item_id = %hold.work_item_id,
                                ?err,
                                "positioning-refusal recheck sweep: hold lowered but did not mint a replacement"
                            );
                        }
                    }
                }
                tracing::info!(work_item_id = %hold.work_item_id, "positioning-refusal recheck sweep: refusal cleared; hold lowered");
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(work_item_id = %hold.work_item_id, ?err, "positioning-refusal recheck sweep: failed to lower hold; keeping it");
            }
        }
    }
    if outcome.cleared > 0 {
        coordinator.kick();
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::pr_is_positionable;
    use crate::work::{PrInspect, PrOpenState};

    fn inspect(open_state: PrOpenState, head_sha: Option<&str>, head_ref_name: Option<&str>) -> PrInspect {
        PrInspect {
            open_state,
            head_sha: head_sha.map(str::to_owned),
            head_ref_name: head_ref_name.map(str::to_owned),
        }
    }

    #[test]
    fn only_an_open_pr_with_a_full_head_and_branch_name_is_positionable() {
        let sha = "a".repeat(40);
        assert!(pr_is_positionable(&inspect(
            PrOpenState::Open,
            Some(&sha),
            Some("feature")
        )));
        assert!(!pr_is_positionable(&inspect(
            PrOpenState::Merged,
            Some(&sha),
            Some("feature")
        )));
        assert!(!pr_is_positionable(&inspect(
            PrOpenState::ClosedUnmerged,
            Some(&sha),
            Some("feature")
        )));
        assert!(!pr_is_positionable(&inspect(PrOpenState::Open, None, Some("feature"))));
        assert!(!pr_is_positionable(&inspect(
            PrOpenState::Open,
            Some("abc"),
            Some("feature")
        )));
        assert!(!pr_is_positionable(&inspect(PrOpenState::Open, Some(&sha), None)));
        assert!(!pr_is_positionable(&inspect(PrOpenState::Open, Some(&sha), Some(""))));
    }
}
