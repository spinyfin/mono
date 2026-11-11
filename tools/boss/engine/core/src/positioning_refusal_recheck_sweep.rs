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
//! The probe mirrors the checks `cube workspace goto --pr` makes before it
//! touches the repository: the PR is open, and GitHub reports a head ref and a
//! full 40-character head commit. Items with no PR to probe stay held until an
//! explicit launch.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use boss_protocol::RequestExecutionInput;

use crate::coordinator::ExecutionCoordinator;
use crate::work::WorkDb;

/// Asks whether cube would now accept positioning on `pr_url`.
pub type PositioningProbe = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<bool>> + Send>> + Send + Sync>;

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

/// The production probe: ask GitHub for the PR and apply cube's own acceptance
/// conditions to the answer.
pub fn github_positioning_probe() -> PositioningProbe {
    Arc::new(|pr_url| Box::pin(async move { github_pr_positionable(&pr_url).await }))
}

async fn github_pr_positionable(pr_url: &str) -> Result<bool> {
    let (owner, repo, number) =
        boss_github::pr_url::parse_pr_url_parts(pr_url).with_context(|| format!("not a canonical PR url: {pr_url}"))?;
    let display = format!("gh api repos/{owner}/{repo}/pulls/{number}");
    let body =
        boss_github::gh_runner::run_gh(&["api", &format!("repos/{owner}/{repo}/pulls/{number}")], &display).await?;
    let pr: serde_json::Value = serde_json::from_str(&body).context("unparseable PR response")?;
    Ok(pr_is_positionable(&pr))
}

/// Cube's `goto --pr` acceptance conditions, applied to GitHub's PR payload.
fn pr_is_positionable(pr: &serde_json::Value) -> bool {
    let open = pr["state"] == "open" && pr["merged"] != true;
    let head_ref = pr["head"]["ref"].as_str().is_some_and(|name| !name.is_empty());
    let full_sha = pr["head"]["sha"]
        .as_str()
        .is_some_and(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()));
    open && head_ref && full_sha
}

/// Spawn the sweep, firing on spawn and then every `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    probe: PositioningProbe,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let probe = Arc::clone(&probe);
        async move { run_one_pass(&work_db, &coordinator, &probe).await }
    })
}

/// Run one recheck pass over every held item.
pub async fn run_one_pass(
    work_db: &WorkDb,
    coordinator: &Arc<ExecutionCoordinator>,
    probe: &PositioningProbe,
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
        let Some(pr_url) = hold.pr_url.clone() else {
            continue;
        };
        match probe(pr_url).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(err) => {
                tracing::warn!(work_item_id = %hold.work_item_id, ?err, "positioning-refusal recheck sweep: probe failed; keeping hold");
                outcome.probe_errors += 1;
                continue;
            }
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
    use serde_json::json;

    #[test]
    fn only_an_open_pr_with_a_full_head_is_positionable() {
        let sha = "a".repeat(40);
        assert!(pr_is_positionable(
            &json!({"state":"open","head":{"ref":"b","sha":sha}})
        ));
        assert!(!pr_is_positionable(
            &json!({"state":"closed","merged":true,"head":{"ref":"b","sha":sha}})
        ));
        assert!(!pr_is_positionable(
            &json!({"state":"open","head":{"ref":"","sha":sha}})
        ));
        assert!(!pr_is_positionable(
            &json!({"state":"open","head":{"ref":"b","sha":"abc"}})
        ));
    }
}
