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
    /// The PR was enqueued in the repository's (GitHub-native) merge queue.
    Enqueued,
    /// Auto-merge was enabled; the PR will merge once required checks pass.
    AutoMergeEnabled,
    /// The PR was merged directly (all checks were already passing and no
    /// merge queue was configured for this PR).
    Merged,
    /// The PR was submitted to a `trunk_queue`-mechanism product's Trunk
    /// merge queue (`POST submitPullRequest`). Produced by
    /// `app::review::handle_merge_when_ready`'s `MergeMechanism::TrunkQueue`
    /// branch, not by [`gh_merge_when_ready`] — see
    /// `trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`
    /// §"The merge verb: submit + standing merge intent".
    TrunkEnqueued,
}

impl MergeAction {
    /// Stable snake_case string sent over the wire in
    /// `FrontendEvent::MergeWhenReadyAccepted`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Requested => "merge_requested",
            Self::Enqueued => "enqueued",
            Self::AutoMergeEnabled => "auto_merge_enabled",
            Self::Merged => "merged",
            Self::TrunkEnqueued => "trunk_enqueued",
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
    let head_sha = probe_head_sha(pr_url).await?;
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
    fn merge_action_enqueued_as_str() {
        assert_eq!(MergeAction::Enqueued.as_str(), "enqueued");
    }

    #[test]
    fn merge_action_requested_as_str() {
        assert_eq!(MergeAction::Requested.as_str(), "merge_requested");
    }

    #[test]
    fn merge_action_auto_merge_enabled_as_str() {
        assert_eq!(MergeAction::AutoMergeEnabled.as_str(), "auto_merge_enabled");
    }

    #[test]
    fn merge_action_merged_as_str() {
        assert_eq!(MergeAction::Merged.as_str(), "merged");
    }

    #[test]
    fn merge_action_trunk_enqueued_as_str() {
        assert_eq!(MergeAction::TrunkEnqueued.as_str(), "trunk_enqueued");
    }
}
