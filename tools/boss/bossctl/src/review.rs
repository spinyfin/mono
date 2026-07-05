//! `bossctl review` — operator control verbs for the automated review
//! pipeline.
//!
//! `review start --pr <n>` re-enqueues a `pr_review` execution for an
//! open PR on demand: the same dispatch path the engine's dead-review
//! auto-recovery sweep (`boss_engine::pr_review_recovery`) uses when a
//! reviewer dies mid-run. Useful for post-hoc review after an incident, or
//! a deliberate re-review after significant new commits.

use anyhow::{Context, Result, bail};
use boss_protocol::{FrontendEvent, FrontendRequest};
use clap::Subcommand;

use super::connect;

#[derive(Subcommand, Debug)]
pub(crate) enum ReviewAction {
    /// Re-enqueue the automated review pipeline for an open PR.
    ///
    /// Enqueues a fresh `pr_review` execution against the work item bound
    /// to `--pr`, the same dispatch path the engine's dead-review
    /// auto-recovery sweep uses. Useful for post-hoc review after an
    /// incident (a prior reviewer died without producing findings, e.g.
    /// dispatched to a broken host) or a deliberate re-review after
    /// significant new commits. Refuses if the PR is merged/closed, the
    /// work item is terminal, or an execution is already live on it.
    Start {
        /// GitHub PR number (e.g. `1758` for `.../pull/1758`).
        #[arg(long = "pr")]
        pr_number: i64,
        /// Disambiguate when the same PR number exists in more than one
        /// repo. Matched as a substring against the bound PR URL.
        #[arg(long)]
        repo: Option<String>,
    },
}

pub(crate) async fn review_start(
    socket_path: &Option<String>,
    json: bool,
    pr_number: i64,
    repo: Option<String>,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::TriggerPrReview { pr_number, repo })
        .await
        .context("sending TriggerPrReview")?;
    match response {
        FrontendEvent::PrReviewTriggered {
            execution,
            work_item_id,
            pr_url,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "execution": &execution,
                        "work_item_id": &work_item_id,
                        "pr_url": &pr_url,
                    }))
                    .expect("response serializes")
                );
            } else {
                println!("re-enqueued review for PR #{pr_number}");
                println!("  work item: {work_item_id}");
                println!("  pr url:    {pr_url}");
                println!("  execution: {}  [{}]", execution.id, execution.status);
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected review start: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}
