//! Engine-side "Merge When Ready" action.
//!
//! Implements the `MergeWhenReady` RPC: given a PR URL, fires
//! `gh pr merge --auto --squash` which handles all three cases:
//! - repo has a merge queue → enqueues the PR
//! - no merge queue, all required checks pass → merges directly
//! - no merge queue, checks still pending → enables auto-merge
//!
//! The command is pinned to the PR head observed immediately before it runs.
//! GitHub materializes merge-queue and auto-merge fields asynchronously, so
//! the caller records the successful request instead of treating an empty
//! immediate post-command probe as evidence that the request disappeared.

use anyhow::{Result, anyhow};
use async_trait::async_trait;

use boss_engine_gh_invocation::gh_output;
use boss_gh_telemetry::{callers, scope as gh_scope};

/// Outcome of a successful merge-when-ready call, whichever mechanism
/// produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeAction {
    /// GitHub accepted a Merge When Ready request. This is intentionally the
    /// Direct-path response while GitHub's derived queue/auto-merge state is
    /// still converging; it is accurate without a replica-lagged probe.
    Requested,
    /// The PR was submitted to a `trunk_queue`-mechanism product's Trunk
    /// merge queue (`POST submitPullRequest`). Produced by
    /// `app::review::handle_merge_when_ready`'s `MergeMechanism::TrunkQueue`
    /// branch, not by [`gh_merge_when_ready`] — see
    /// `trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`
    /// §"The merge verb: submit + standing merge intent".
    TrunkEnqueued,
    /// A subsequent Merge click found an already-active intent that is
    /// still live in the Trunk queue (or was just submitted and not yet
    /// observed). Distinct from [`Self::TrunkEnqueued`]: this click did
    /// **not** call `submitPullRequest`.
    TrunkAlreadyEnqueued,
}

impl MergeAction {
    /// Stable snake_case string sent over the wire in
    /// `FrontendEvent::MergeWhenReadyAccepted`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Requested => "merge_requested",
            Self::TrunkEnqueued => "trunk_enqueued",
            Self::TrunkAlreadyEnqueued => "trunk_already_enqueued",
        }
    }
}

/// The durable facts returned by the GitHub-native merge command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectMergeSuccess {
    /// The PR head guarded by `gh pr merge --match-head-commit`.
    pub head_sha: String,
}

/// Perform "Merge When Ready" for `pr_url`.
///
/// Shells out to `gh pr merge --auto --squash <pr_url>`, binding the request
/// to the current head SHA. Returns that SHA on success or an `Err` carrying
/// the `gh` error message when the merge was rejected (conflicts, auth
/// failure, PR not open, etc.).
pub async fn gh_merge_when_ready(pr_url: &str) -> Result<DirectMergeSuccess> {
    let started = std::time::Instant::now();
    let head_sha = probe_head_sha(pr_url).await?;
    tracing::info!(
        pr_url,
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "merge request head probe completed"
    );
    let command_started = std::time::Instant::now();
    let output = gh_scope(
        callers::MERGE_WHEN_READY,
        gh_output(&[
            "pr",
            "merge",
            "--auto",
            "--squash",
            "--match-head-commit",
            &head_sha,
            pr_url,
        ]),
    )
    .await
    .map_err(|e| anyhow!("failed to spawn `gh pr merge`: {e}"))?;
    tracing::info!(
        pr_url,
        elapsed_ms = command_started.elapsed().as_secs_f64() * 1000.0,
        success = output.status.success(),
        "merge request GitHub command completed"
    );

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{}{}", stderr.trim(), stdout.trim());
        return Err(anyhow!("gh pr merge failed: {}", combined.trim()));
    }

    Ok(DirectMergeSuccess { head_sha })
}

/// Executes the Direct merge-mechanism side effect (`gh pr merge --auto
/// --squash`, via [`gh_merge_when_ready`]) for a PR. `app::review`'s
/// `handle_merge_when_ready` calls this instead of the free function
/// directly so test doubles can stub the live `gh` call — see
/// `CommandDirectMergeExecutor` for the production impl and
/// `app::review`'s `trunk_queue_tests` for a fake used by the Direct-branch
/// routing tests.
#[async_trait]
pub trait DirectMergeExecutor: Send + Sync {
    async fn execute(&self, pr_url: &str) -> Result<DirectMergeSuccess>;
}

/// `DirectMergeExecutor` that shells out to `gh pr merge --auto --squash`
/// via [`gh_merge_when_ready`]. The production default everywhere except
/// tests that inject a fake through `ServerState`'s
/// `direct_merge_executor_override`.
#[derive(Debug, Default)]
pub struct CommandDirectMergeExecutor;

#[async_trait]
impl DirectMergeExecutor for CommandDirectMergeExecutor {
    async fn execute(&self, pr_url: &str) -> Result<DirectMergeSuccess> {
        gh_merge_when_ready(pr_url).await
    }
}

/// Run the shared checked merge and surface repository push restrictions.
pub async fn execute_direct_merge(
    db: &crate::work::WorkDb,
    executor: &dyn DirectMergeExecutor,
    task_id: &str,
    product: &boss_protocol::Product,
    pr_url: &str,
) -> Result<DirectMergeSuccess> {
    let result = executor.execute(pr_url).await;
    if let Err(err) = &result {
        let message = format!("{err:#}");
        if crate::merge_mechanism::is_push_restriction_error(&message) {
            db.create_attention_item(crate::work::CreateAttentionItemInput {
                work_item_id: Some(task_id.to_owned()),
                kind: crate::merge_mechanism::PUSH_RESTRICTION_ATTENTION_KIND.to_owned(),
                title: crate::merge_mechanism::PUSH_RESTRICTION_ATTENTION_TITLE.to_owned(),
                body_markdown: crate::merge_mechanism::render_push_restriction_attention_body(
                    &product.name,
                    &product.slug,
                    &message,
                ),
                execution_id: None,
                status: None,
                resolved_at: None,
            })?;
        }
    }
    result
}

/// Persist acceptance before acknowledging it or publishing card changes.
pub fn record_direct_merge(
    db: &crate::work::WorkDb,
    task_id: &str,
    pr_url: &str,
    success: &DirectMergeSuccess,
) -> Result<crate::work::GithubMergeIntentRecordOutcome> {
    let detail = serde_json::json!({"position": null, "state": null, "enqueued_at": null,
        "section_order": boss_engine_merge_poller::QUEUED_NO_POSITION_SECTION_ORDER})
    .to_string();
    db.record_github_merge_intent(
        crate::work::GithubMergeIntentInsertInput::builder()
            .work_item_id(task_id)
            .pr_url(pr_url)
            .head_sha(&success.head_sha)
            .build(),
        &detail,
    )
}

/// Wake reconciliation immediately and publish the durable queue transition.
pub async fn publish_direct_merge(
    db: &crate::work::WorkDb,
    publisher: &dyn boss_execution_contracts::ExecutionPublisher,
    product_id: &str,
    task_id: &str,
    pr_url: &str,
    state_changed: bool,
) {
    db.event_bus().publish(boss_event_bus::Event::PrReconcileRequested {
        pr_url: pr_url.to_owned(),
    });
    if state_changed {
        publisher
            .publish_work_item_changed(product_id, task_id, "gh_merge_queue_submitted")
            .await;
    }
}

/// Return the exact PR head the subsequent merge command must match.
async fn probe_head_sha(pr_url: &str) -> Result<String> {
    let output = gh_scope(
        callers::MERGE_WHEN_READY,
        gh_output(&["pr", "view", pr_url, "--json", "headRefOid", "--jq", ".headRefOid"]),
    )
    .await
    .map_err(|err| anyhow!("failed to spawn `gh pr view` for merge head: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "failed to read PR head before merge: {}",
            format!("{}{}", stderr.trim(), stdout.trim()).trim()
        ));
    }
    let head_sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if head_sha.is_empty() {
        return Err(anyhow!(
            "failed to read PR head before merge: GitHub returned an empty head SHA"
        ));
    }
    Ok(head_sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- MergeAction::as_str ---

    #[test]
    fn merge_action_requested_as_str() {
        assert_eq!(MergeAction::Requested.as_str(), "merge_requested");
    }

    #[test]
    fn merge_action_trunk_enqueued_as_str() {
        assert_eq!(MergeAction::TrunkEnqueued.as_str(), "trunk_enqueued");
    }

    #[test]
    fn merge_action_trunk_already_enqueued_as_str() {
        assert_eq!(MergeAction::TrunkAlreadyEnqueued.as_str(), "trunk_already_enqueued");
    }
}
