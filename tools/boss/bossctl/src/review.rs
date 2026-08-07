//! `bossctl review` — operator control and inspection verbs for the
//! automated review pipeline.
//!
//! `review start --pr <n>` re-enqueues a `pr_review` execution for an
//! open PR on demand: the same dispatch path the engine's dead-review
//! auto-recovery sweep (`boss_engine::pr_review_recovery`) uses when a
//! reviewer dies mid-run. Useful for post-hoc review after an incident, or
//! a deliberate re-review after significant new commits.
//!
//! `review show <work-item>` is the read counterpart: it answers "was this
//! PR actually reviewed, and what did the reviewer conclude" from the
//! durable `pr_review_verdicts` row(s) the engine already writes, without a
//! direct database query. See its own doc comment below for why it lives
//! under `bossctl` rather than `boss` — the coordinator, not a worker, is
//! the caller who needs this.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use boss_engine::work::{ReviewVerdict, is_informative_gate_outcome};
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
    /// Show the review-verdict history for a work item: has it been
    /// reviewed, did it come back clean or with findings, and is the
    /// reviewed head still the PR's current head.
    ///
    /// Read-only — never mutates a verdict, re-enqueues a review, or
    /// resolves an attention item. Reads `pr_review_verdicts` (and the PR's
    /// last-polled head sha) directly out of `state.db`, the same
    /// direct-DB pattern `bossctl work executions` already uses, so it
    /// still answers even when the engine itself is wedged.
    Show {
        /// Work item id (task/chore), e.g. `task_abc123`.
        work_item: String,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

/// `bossctl review show` — see [`ReviewAction::Show`].
///
/// Lives under `bossctl review` (a sibling of `review start`, which already
/// owns the review-pipeline verb namespace) rather than under `boss`: the
/// motivating case — "why was this task not reviewed" — is a coordinator
/// question, and `bossctl` is the Boss-only CLI the coordinator session
/// runs, not a worker-facing surface (see `bossctl`'s crate doc comment).
pub(crate) fn review_show(json: bool, state_root: Option<PathBuf>, work_item: String) -> Result<()> {
    let db = super::open_state_db(state_root)?;
    let history = db
        .review_verdicts_for_work_item(&work_item)
        .context("reading review verdict history")?;
    let latest_informative = history.iter().find(|v| is_informative_gate_outcome(&v.gate_outcome));
    let pr_status = db
        .get_pr_status_snapshot(&work_item)
        .context("reading PR status snapshot")?;
    let current_head_sha = pr_status.as_ref().and_then(|s| s.head_sha.clone());
    // `None` means "can't tell" (no informative verdict yet, or the PR head
    // has never been polled) — deliberately distinct from `Some(false)`, so
    // a caller never confuses "unknown" with "confirmed current".
    let reviewed_head_is_stale = match (latest_informative, &current_head_sha) {
        (Some(v), Some(current)) => v.head_sha.as_deref().map(|reviewed| reviewed != current),
        _ => None,
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "work_item_id": work_item,
                "pr_url": pr_status.as_ref().and_then(|s| s.pr_url.clone()),
                "current_head_sha": current_head_sha,
                "latest_informative_verdict": latest_informative.map(verdict_json),
                "reviewed_head_is_stale": reviewed_head_is_stale,
                "history": history.iter().map(verdict_json_with_informative_flag).collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }

    println!("work item: {work_item}");
    match pr_status.as_ref().and_then(|s| s.pr_url.as_deref()) {
        Some(pr_url) => {
            println!("  pr:           {pr_url}");
            println!(
                "  current head: {}",
                current_head_sha.as_deref().unwrap_or("(not yet probed)")
            );
        }
        None => println!("  pr: (none bound to this work item yet)"),
    }
    println!();

    match latest_informative {
        None if history.is_empty() => {
            println!("never reviewed — no pr_review pass has completed for this work item");
        }
        None => println!(
            "no informative verdict — {} pass(es) recorded, all gave_up or dropped_duplicate_head",
            history.len()
        ),
        Some(v) => {
            println!("latest informative verdict:");
            println!("  outcome:            {}", v.gate_outcome);
            println!("  findings:           {}", v.findings_count);
            println!("  revision warranted: {}", v.revision_warranted);
            println!(
                "  revision task:      {}",
                v.revision_task_id.as_deref().unwrap_or("(none)")
            );
            println!("  reviewed head:      {}", v.head_sha.as_deref().unwrap_or("(unknown)"));
            println!("  recorded at:        {}", v.created_at);
            match reviewed_head_is_stale {
                Some(true) => println!(
                    "  ** STALE — the PR has moved since this review; current head is {} **",
                    current_head_sha.as_deref().unwrap_or("(unknown)")
                ),
                Some(false) => println!("  reviewed head matches the PR's current head"),
                None => {}
            }
        }
    }

    if history.len() > 1 {
        println!();
        println!("full history ({} pass(es), most recent first):", history.len());
        for v in &history {
            let tag = if is_informative_gate_outcome(&v.gate_outcome) {
                "informative"
            } else {
                "skipped    "
            };
            println!(
                "  [{tag}] {:<26} findings={:<3} head={:<10} {}",
                v.gate_outcome,
                v.findings_count,
                v.head_sha.as_deref().unwrap_or("-"),
                v.created_at,
            );
        }
    }

    Ok(())
}

fn verdict_json(v: &ReviewVerdict) -> serde_json::Value {
    serde_json::json!({
        "id": v.id,
        "execution_id": v.execution_id,
        "work_item_id": v.work_item_id,
        "head_sha": v.head_sha,
        "findings_count": v.findings_count,
        "revision_warranted": v.revision_warranted,
        "gate_outcome": v.gate_outcome,
        "revision_task_id": v.revision_task_id,
        "created_at": v.created_at,
    })
}

fn verdict_json_with_informative_flag(v: &ReviewVerdict) -> serde_json::Value {
    let mut value = verdict_json(v);
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "informative".to_owned(),
            serde_json::Value::Bool(is_informative_gate_outcome(&v.gate_outcome)),
        );
    }
    value
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
