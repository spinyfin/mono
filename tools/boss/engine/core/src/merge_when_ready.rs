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

/// Continue the user's standing request, never a merge inferred from readiness.
/// Called only after a definitive clean probe. Keeping the request in the DB
/// also recovers the gap between a successful rebase and an engine restart.
pub async fn resume_after_conflict(
    db: &crate::work::WorkDb,
    publisher: &dyn boss_execution_contracts::ExecutionPublisher,
    candidate: &crate::work::PendingMergeCheck,
    executor: &dyn DirectMergeExecutor,
) {
    let result: Result<bool> = async {
        if !db.merge_conflict_resolution_requested(&candidate.work_item_id, &candidate.pr_url)? {
            return Ok(false);
        }
        // A clean observation can race a mechanical push. Wait until the
        // resolver has finished its safety checks and released the attempt.
        if db
            .active_conflict_resolution_for_work_item(&candidate.work_item_id)?
            .is_some()
        {
            return Ok(false);
        }
        if !db.open_merge_revisions(&candidate.work_item_id)?.is_empty() {
            return db.stop_merge_conflict_resolution(&candidate.work_item_id, "Conflict resolution needs a revision");
        }
        // Never run the merge command unless the intent can be recorded for it.
        if !db.merge_conflict_resolution_ready(&candidate.work_item_id, &candidate.pr_url)? {
            return Ok(false);
        }
        match executor.execute(&candidate.pr_url).await {
            Ok(success) => {
                let finished =
                    db.finish_merge_conflict_resolution(&candidate.work_item_id, &candidate.pr_url, &success.head_sha)?;
                if !finished {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        "merge command succeeded but the card moved on before its intent was recorded"
                    );
                    return Ok(false);
                }
            }
            Err(err) => {
                db.stop_merge_conflict_resolution(
                    &candidate.work_item_id,
                    &format!("Merge failed after conflict resolution: {err:#}"),
                )?;
            }
        }
        Ok(true)
    }
    .await;
    match result {
        Ok(true) => {
            publisher
                .publish_work_item_changed(
                    &candidate.product_id,
                    &candidate.work_item_id,
                    "merge_conflict_continuation",
                )
                .await
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(?err, work_item_id = %candidate.work_item_id, "could not continue merge after conflict resolution")
        }
    }
}

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
    use crate::work::{ConflictResolutionInsertInput, PendingMergeCheck, WorkDb, WorkItemPatch};
    use boss_execution_contracts::NoopExecutionPublisher;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ResumeExecutor {
        calls: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl DirectMergeExecutor for ResumeExecutor {
        async fn execute(&self, _: &str) -> Result<DirectMergeSuccess> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("GitHub rejected the merge");
            }
            Ok(DirectMergeSuccess {
                head_sha: "resolved-head".into(),
            })
        }
    }

    #[tokio::test]
    async fn merge_resumes_once_only_after_the_requested_resolution_finishes() {
        for fail in [false, true] {
            let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
            let product = boss_work_db::test_support::create_test_product_named(&db, "Merge continuation");
            let task = boss_work_db::test_support::create_test_chore_manual(&db, product.id.clone(), "Merge");
            let candidate = PendingMergeCheck {
                work_item_id: task.id.clone(),
                product_id: product.id.clone(),
                pr_url: "https://github.com/example/repo/pull/42".into(),
            };
            db.update_work_item(
                &task.id,
                WorkItemPatch {
                    status: Some("in_review".into()),
                    pr_url: Some(candidate.pr_url.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
            let executor = ResumeExecutor {
                calls: AtomicUsize::new(0),
                fail,
            };
            resume_after_conflict(&db, &NoopExecutionPublisher, &candidate, &executor).await;
            assert_eq!(
                executor.calls.load(Ordering::SeqCst),
                0,
                "readiness alone cannot authorize merging"
            );
            db.start_merge_conflict_resolution(&task.id, &candidate.pr_url, "main")
                .unwrap();
            db.mark_chore_blocked_merge_conflict(&task.id, &candidate.pr_url)
                .unwrap();
            let attempt = db
                .insert_conflict_resolution(
                    ConflictResolutionInsertInput::builder()
                        .product_id(&product.id)
                        .work_item_id(&task.id)
                        .pr_url(&candidate.pr_url)
                        .pr_number(42)
                        .head_branch("feature")
                        .base_branch("main")
                        .base_sha_at_trigger("base")
                        .head_sha_before("old-head")
                        .build(),
                )
                .unwrap()
                .unwrap();
            resume_after_conflict(&db, &NoopExecutionPublisher, &candidate, &executor).await;
            assert_eq!(
                executor.calls.load(Ordering::SeqCst),
                0,
                "do not race the resolver's safety checks"
            );
            db.clear_chore_blocked_merge_conflict(&task.id, &candidate.pr_url)
                .unwrap();
            db.mark_conflict_resolution_succeeded_at_rung(&attempt.id, Some("resolved-head"), 1)
                .unwrap();
            resume_after_conflict(&db, &NoopExecutionPublisher, &candidate, &executor).await;
            resume_after_conflict(&db, &NoopExecutionPublisher, &candidate, &executor).await;
            assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
            let card = db
                .get_work_tree(&product.id)
                .unwrap()
                .chores
                .into_iter()
                .find(|t| t.id == task.id)
                .unwrap();
            if fail {
                assert_eq!(card.merge_queue_state, None);
                assert!(card.merge_queue_detail.unwrap().contains("GitHub rejected the merge"));
            } else {
                assert_eq!(card.merge_queue_state.as_deref(), Some("queued"));
                assert_eq!(
                    db.get_active_github_merge_intent(&task.id, &candidate.pr_url)
                        .unwrap()
                        .unwrap()
                        .head_sha,
                    "resolved-head"
                );
            }
        }
    }

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
