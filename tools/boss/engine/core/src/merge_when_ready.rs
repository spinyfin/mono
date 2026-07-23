//! Engine-side "Merge When Ready" action.
//!
//! Implements the `MergeWhenReady` RPC: given a PR URL, fires
//! `gh pr merge --auto --squash` which handles all three cases:
//! - repo has a merge queue → enqueues the PR
//! - no merge queue, all required checks pass → merges directly
//! - no merge queue, checks still pending → enables auto-merge
//!
//! After a successful merge call the PR state is re-probed to determine
//! which of the three paths was taken so the caller can surface a precise
//! status label (`enqueued` / `merged` / `auto_merge_enabled`).

use anyhow::{Result, anyhow};
use async_trait::async_trait;

use boss_github::gh_runner::pr_in_merge_queue;

use boss_engine_gh_invocation::gh_output;

/// Outcome of a successful merge-when-ready call, whichever mechanism
/// produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeAction {
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
            Self::Enqueued => "enqueued",
            Self::AutoMergeEnabled => "auto_merge_enabled",
            Self::Merged => "merged",
            Self::TrunkEnqueued => "trunk_enqueued",
        }
    }
}

/// Perform "Merge When Ready" for `pr_url`.
///
/// Shells out to `gh pr merge --auto --squash <pr_url>` then re-probes
/// the PR state to identify which outcome occurred. Returns
/// [`MergeAction`] on success or an `Err` carrying the `gh` error
/// message when the merge was rejected (conflicts, auth failure, PR not
/// open, etc.).
pub async fn gh_merge_when_ready(pr_url: &str) -> Result<MergeAction> {
    let output = gh_output(&["pr", "merge", "--auto", "--squash", pr_url])
        .await
        .map_err(|e| anyhow!("failed to spawn `gh pr merge`: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{}{}", stderr.trim(), stdout.trim());
        return Err(anyhow!("gh pr merge failed: {}", combined.trim()));
    }

    // Re-probe concurrently to determine which outcome occurred.
    let (is_merged, is_in_queue) = tokio::join!(probe_is_merged(pr_url), pr_in_merge_queue(pr_url),);

    Ok(derive_action(is_in_queue, is_merged))
}

/// Derive the [`MergeAction`] from the post-merge PR state probes.
///
/// Extracted as a pure function so the branch logic is unit-testable
/// without a live `gh` process.
pub(crate) fn derive_action(is_in_queue: bool, is_merged: bool) -> MergeAction {
    if is_in_queue {
        MergeAction::Enqueued
    } else if is_merged {
        MergeAction::Merged
    } else {
        MergeAction::AutoMergeEnabled
    }
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
    async fn execute(&self, pr_url: &str) -> Result<MergeAction>;
}

/// `DirectMergeExecutor` that shells out to `gh pr merge --auto --squash`
/// via [`gh_merge_when_ready`]. The production default everywhere except
/// tests that inject a fake through `ServerState`'s
/// `direct_merge_executor_override`.
#[derive(Debug, Default)]
pub struct CommandDirectMergeExecutor;

#[async_trait]
impl DirectMergeExecutor for CommandDirectMergeExecutor {
    async fn execute(&self, pr_url: &str) -> Result<MergeAction> {
        gh_merge_when_ready(pr_url).await
    }
}

/// Returns `true` when the PR's GitHub state is `MERGED`.
/// Returns `false` on any error (graceful degradation).
async fn probe_is_merged(pr_url: &str) -> bool {
    let output = gh_output(&["pr", "view", pr_url, "--json", "state", "--jq", ".state"]).await;
    let Ok(out) = output else { return false };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout).trim() == "MERGED"
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

    // --- derive_action (mirrors the if/else in gh_merge_when_ready) ---

    /// queue-enabled repo → enqueued (PR ends up in the merge queue)
    #[test]
    fn derive_action_in_queue_yields_enqueued() {
        assert_eq!(derive_action(true, false), MergeAction::Enqueued);
    }

    /// queue-enabled repo where the PR was already in queue AND shows
    /// merged — the queue flag takes precedence.
    #[test]
    fn derive_action_queue_takes_precedence_over_merged() {
        assert_eq!(derive_action(true, true), MergeAction::Enqueued);
    }

    /// non-queue repo, checks already green → direct merge
    #[test]
    fn derive_action_not_in_queue_but_merged_yields_merged() {
        assert_eq!(derive_action(false, true), MergeAction::Merged);
    }

    /// non-queue repo, checks still pending → auto-merge enabled
    #[test]
    fn derive_action_not_in_queue_not_merged_yields_auto_merge_enabled() {
        assert_eq!(derive_action(false, false), MergeAction::AutoMergeEnabled);
    }
}
