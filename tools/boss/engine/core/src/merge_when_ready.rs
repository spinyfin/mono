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
    gh_merge_when_ready_matching_head(pr_url, None).await
}

/// Perform "Merge When Ready" for `pr_url`, optionally pinning
/// `--match-head-commit` to an already-verified `expected_head_sha` without
/// re-probing GitHub.
///
/// When `expected_head_sha` is `Some` (non-empty), `gh pr merge` fails before
/// GitHub accepts anything if GitHub's view of the PR head does not match
/// that SHA (preventing a lagged probe from re-introducing a pre-rebase SHA).
/// When `None`, the current head is probed first as in [`gh_merge_when_ready`].
pub async fn gh_merge_when_ready_matching_head(
    pr_url: &str,
    expected_head_sha: Option<&str>,
) -> Result<DirectMergeSuccess> {
    let head_sha = match expected_head_sha.filter(|s| !s.is_empty()) {
        Some(sha) => sha.to_owned(),
        None => {
            let started = std::time::Instant::now();
            let head_sha = probe_head_sha(pr_url).await?;
            tracing::info!(
                pr_url,
                elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                "merge request head probe completed"
            );
            head_sha
        }
    };
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

/// Turn off GitHub auto-merge for `pr_url` (`gh pr merge --disable-auto`).
pub async fn gh_disarm_auto_merge(pr_url: &str) -> Result<()> {
    let output = gh_scope(
        callers::MERGE_WHEN_READY,
        gh_output(&["pr", "merge", "--disable-auto", pr_url]),
    )
    .await
    .map_err(|e| anyhow!("failed to spawn `gh pr merge --disable-auto`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "gh pr merge --disable-auto failed: {}",
            format!("{}{}", stderr.trim(), stdout.trim()).trim()
        ));
    }
    Ok(())
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

    /// Like [`Self::execute`], but when `expected_head_sha` is `Some`, pins
    /// `--match-head-commit` directly to that SHA without re-probing GitHub.
    async fn execute_matching_head(&self, pr_url: &str, expected_head_sha: Option<&str>) -> Result<DirectMergeSuccess> {
        let _ = expected_head_sha;
        self.execute(pr_url).await
    }

    /// Turn off the auto-merge a successful [`Self::execute`] armed, so a PR
    /// the engine has stopped merging cannot be merged by GitHub anyway.
    async fn disarm(&self, pr_url: &str) -> Result<()>;

    /// Whether GitHub currently has auto-merge armed for the PR. Consulted
    /// when [`Self::disarm`] fails, to tell "nothing was left to turn off"
    /// from a real failure. Executors that cannot tell report an error, which
    /// is treated as "still armed".
    async fn auto_merge_enabled(&self, _pr_url: &str) -> Result<bool> {
        Err(anyhow!("this executor cannot read auto-merge state"))
    }
}

/// `DirectMergeExecutor` that shells out to `gh pr merge --auto --squash`
/// via [`gh_merge_when_ready`]. The production default everywhere except
/// tests that inject a fake through `ServerState`'s
/// `direct_merge_executor_override`.
#[derive(Debug, Default)]
pub struct CommandDirectMergeExecutor;

/// Finish stopping a merge request whose GitHub auto-merge was still armed
/// when conflict resolution gave up: turn auto-merge off. A failure is shown
/// on the card (the pause is not real yet) and retried on the next sweep.
pub async fn disarm_stopped_merge(
    db: &crate::work::WorkDb,
    publisher: &dyn boss_execution_contracts::ExecutionPublisher,
    candidate: &crate::work::PendingMergeCheck,
    executor: &dyn DirectMergeExecutor,
) {
    let outcome: Result<Option<&'static str>> = async {
        if !db.merge_disarm_pending(&candidate.work_item_id, &candidate.pr_url)? {
            return Ok(None);
        }
        if db
            .get_active_github_merge_intent(&candidate.work_item_id, &candidate.pr_url)?
            .is_some()
        {
            return Ok(None);
        }
        match executor.disarm(&candidate.pr_url).await {
            Ok(()) => {
                // The newer request this disarm raced with, captured before
                // the re-arm so a failure retires exactly that request.
                if let Some(rearmed) =
                    db.get_active_github_merge_intent(&candidate.work_item_id, &candidate.pr_url)?
                {
                    match executor.execute(&candidate.pr_url).await {
                        Ok(_) => {
                            let _ = db.complete_merge_disarm(&candidate.work_item_id, &candidate.pr_url)?;
                            Ok(None)
                        }
                        Err(err) => {
                            tracing::warn!(
                                work_item_id = %candidate.work_item_id,
                                pr_url = %candidate.pr_url,
                                ?err,
                                "turned off auto-merge while a newer merge request was in flight, and could not turn it back on"
                            );
                            let _ = db.complete_merge_disarm(&candidate.work_item_id, &candidate.pr_url)?;
                            Ok(db
                                .record_merge_rearm_failure(
                                    &candidate.work_item_id,
                                    &candidate.pr_url,
                                    &rearmed.id,
                                    &format!("{err:#}"),
                                )?
                                .then_some("merge_rearm_failed"))
                        }
                    }
                } else {
                    Ok(db
                        .complete_merge_disarm(&candidate.work_item_id, &candidate.pr_url)?
                        .then_some("merge_disarm_completed"))
                }
            }
            // A failed `--disable-auto` can only mean there was nothing to turn
            // off (auto-merge was switched off elsewhere): ask GitHub rather
            // than guess from the error wording, which is not a stable contract.
            Err(err) if matches!(executor.auto_merge_enabled(&candidate.pr_url).await, Ok(false)) => {
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    ?err,
                    "auto-merge is already off on GitHub; disarm obligation met"
                );
                Ok(db
                    .complete_merge_disarm(&candidate.work_item_id, &candidate.pr_url)?
                    .then_some("merge_disarm_completed"))
            }
            Err(err) => {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    ?err,
                    "could not turn off auto-merge after conflict resolution stopped"
                );
                Ok(db
                    .record_merge_disarm_failure(&candidate.work_item_id, &format!("{err:#}"))?
                    .then_some("merge_disarm_failed"))
            }
        }
    }
    .await;
    match outcome {
        Ok(Some(reason)) => {
            publisher
                .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, reason)
                .await
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(?err, work_item_id = %candidate.work_item_id, "could not stop GitHub auto-merge")
        }
    }
}

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
        if db.is_parked_or_assume_not(&candidate.work_item_id, "resume merge after conflict") {
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
        // Revisions open when the merge was requested were confirmed by the
        // person asking; a newer one may still add commits to the PR.
        if !db.unconfirmed_merge_revisions(&candidate.work_item_id)?.is_empty() {
            return db.stop_merge_conflict_resolution(
                &candidate.work_item_id,
                "A new revision opened while the merge waited for conflict resolution",
            );
        }
        // Never run the merge command unless the intent can be recorded for it.
        if !db.merge_conflict_resolution_ready(&candidate.work_item_id, &candidate.pr_url)? {
            return Ok(false);
        }
        // The request this continuation serves; a different active request
        // seen after the GitHub call is a newer one the person made meanwhile.
        let prior_intent = db
            .get_active_github_merge_intent(&candidate.work_item_id, &candidate.pr_url)?
            .map(|intent| intent.id);
        match executor.execute(&candidate.pr_url).await {
            Ok(success) => {
                match db.finish_merge_conflict_resolution(&candidate.work_item_id, &candidate.pr_url, &success.head_sha)
                {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            work_item_id = %candidate.work_item_id,
                            "merge command succeeded but the card moved on before its intent was recorded"
                        );
                        db.record_unrecorded_merge_disarm(
                            &candidate.work_item_id,
                            &candidate.pr_url,
                            &success.head_sha,
                            prior_intent.as_deref(),
                        )?;
                        return Ok(false);
                    }
                    Err(err) => {
                        if let Err(disarm_err) = db.record_unrecorded_merge_disarm(
                            &candidate.work_item_id,
                            &candidate.pr_url,
                            &success.head_sha,
                            prior_intent.as_deref(),
                        ) {
                            tracing::warn!(
                                work_item_id = %candidate.work_item_id,
                                pr_url = %candidate.pr_url,
                                ?disarm_err,
                                "could not record a disarm obligation after an unrecorded merge"
                            );
                        }
                        return Err(err);
                    }
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

    async fn execute_matching_head(&self, pr_url: &str, expected_head_sha: Option<&str>) -> Result<DirectMergeSuccess> {
        gh_merge_when_ready_matching_head(pr_url, expected_head_sha).await
    }

    async fn disarm(&self, pr_url: &str) -> Result<()> {
        gh_disarm_auto_merge(pr_url).await
    }

    async fn auto_merge_enabled(&self, pr_url: &str) -> Result<bool> {
        gh_auto_merge_enabled(pr_url).await
    }
}

/// Whether GitHub has auto-merge armed for `pr_url` (`autoMergeRequest` is
/// non-null in `gh pr view --json autoMergeRequest`).
pub async fn gh_auto_merge_enabled(pr_url: &str) -> Result<bool> {
    let output = gh_scope(
        callers::MERGE_WHEN_READY,
        gh_output(&[
            "pr",
            "view",
            pr_url,
            "--json",
            "autoMergeRequest",
            "--jq",
            ".autoMergeRequest != null",
        ]),
    )
    .await
    .map_err(|err| anyhow!("failed to spawn `gh pr view` for auto-merge state: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("failed to read auto-merge state: {}", stderr.trim()));
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(anyhow!("unexpected auto-merge state `{other}`")),
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
        disarms: AtomicUsize,
        fail: bool,
        fail_disarms: AtomicUsize,
    }

    impl ResumeExecutor {
        fn new(fail: bool) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                disarms: AtomicUsize::new(0),
                fail,
                fail_disarms: AtomicUsize::new(0),
            }
        }
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

        async fn disarm(&self, _: &str) -> Result<()> {
            self.disarms.fetch_add(1, Ordering::SeqCst);
            if self.fail_disarms.load(Ordering::SeqCst) > 0 {
                self.fail_disarms.fetch_sub(1, Ordering::SeqCst);
                anyhow::bail!("gh pr merge --disable-auto failed: boom");
            }
            Ok(())
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
            let executor = ResumeExecutor::new(fail);
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
            db.park_work_item_as_actor(&task.id, true, Some("pause merge"), "human")
                .unwrap();
            resume_after_conflict(&db, &NoopExecutionPublisher, &candidate, &executor).await;
            assert_eq!(
                executor.calls.load(Ordering::SeqCst),
                0,
                "a parked request cannot resume"
            );
            db.park_work_item_as_actor(&task.id, false, None, "human").unwrap();
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

    #[tokio::test]
    async fn a_stopped_armed_merge_is_disarmed_on_github_once() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = boss_work_db::test_support::create_test_product_named(&db, "Disarm");
        let task = boss_work_db::test_support::create_test_chore_manual(&db, product.id.clone(), "Merge");
        let pr_url = "https://github.com/example/repo/pull/42".to_owned();
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        db.record_github_merge_intent(
            boss_work_db::GithubMergeIntentInsertInput::builder()
                .work_item_id(&task.id)
                .pr_url(&pr_url)
                .head_sha("head")
                .build(),
            "{}",
        )
        .unwrap();
        db.mark_chore_blocked_merge_conflict(&task.id, &pr_url).unwrap();
        db.stop_merge_conflict_resolution(&task.id, "manual_abandon").unwrap();
        let candidate = PendingMergeCheck {
            work_item_id: task.id.clone(),
            product_id: product.id.clone(),
            pr_url,
        };
        let executor = ResumeExecutor::new(false);
        disarm_stopped_merge(&db, &NoopExecutionPublisher, &candidate, &executor).await;
        disarm_stopped_merge(&db, &NoopExecutionPublisher, &candidate, &executor).await;
        assert_eq!(executor.disarms.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_confirmed_open_revision_does_not_stop_the_continuation() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = boss_work_db::test_support::create_test_product_named(&db, "Confirmed revision");
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
        let revision = boss_work_db::test_support::create_test_chore_manual(&db, product.id.clone(), "Revision");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET kind = 'revision', parent_task_id = ?2, status = 'active' WHERE id = ?1",
                rusqlite::params![revision.id, task.id],
            )
            .unwrap();
        let request = db
            .remember_confirmed_merge_revisions(
                &task.id,
                &candidate.pr_url,
                &[boss_protocol::OpenMergeRevision {
                    id: revision.id.clone(),
                    label: "r1".into(),
                    status: "active".into(),
                }],
            )
            .unwrap();
        db.start_merge_conflict_resolution(&task.id, &candidate.pr_url, "main")
            .unwrap();
        assert!(
            db.attach_confirmed_merge_revisions(&task.id, &candidate.pr_url, &request)
                .unwrap()
        );
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
        db.clear_chore_blocked_merge_conflict(&task.id, &candidate.pr_url)
            .unwrap();
        db.mark_conflict_resolution_succeeded_at_rung(&attempt.id, Some("resolved-head"), 1)
            .unwrap();
        let executor = ResumeExecutor::new(false);
        resume_after_conflict(&db, &NoopExecutionPublisher, &candidate, &executor).await;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            db.get_active_github_merge_intent(&task.id, &candidate.pr_url)
                .unwrap()
                .unwrap()
                .head_sha,
            "resolved-head"
        );
    }

    struct StopDuringMerge<'a> {
        db: &'a WorkDb,
        id: &'a str,
    }

    #[async_trait]
    impl DirectMergeExecutor for StopDuringMerge<'_> {
        async fn execute(&self, _: &str) -> Result<DirectMergeSuccess> {
            self.db
                .stop_merge_conflict_resolution(self.id, "Stopped during merge")?;
            Ok(DirectMergeSuccess {
                head_sha: "resolved-head".into(),
            })
        }

        async fn disarm(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn an_unrecorded_continuation_retries_disarm_after_a_failure() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = boss_work_db::test_support::create_test_product_named(&db, "Unrecorded");
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
        db.start_merge_conflict_resolution(&task.id, &candidate.pr_url, "main")
            .unwrap();
        resume_after_conflict(
            &db,
            &NoopExecutionPublisher,
            &candidate,
            &StopDuringMerge { db: &db, id: &task.id },
        )
        .await;
        assert!(
            db.merge_disarm_pending(&task.id, &candidate.pr_url).unwrap(),
            "the unrecorded GitHub merge must stay a durable disarm obligation"
        );
        let retry = ResumeExecutor::new(false);
        retry.fail_disarms.store(1, Ordering::SeqCst);
        disarm_stopped_merge(&db, &NoopExecutionPublisher, &candidate, &retry).await;
        assert!(db.merge_disarm_pending(&task.id, &candidate.pr_url).unwrap());
        assert_eq!(retry.disarms.load(Ordering::SeqCst), 1);
        disarm_stopped_merge(&db, &NoopExecutionPublisher, &candidate, &retry).await;
        assert!(!db.merge_disarm_pending(&task.id, &candidate.pr_url).unwrap());
        assert_eq!(retry.disarms.load(Ordering::SeqCst), 2);
    }

    /// Records a fresh merge request while the continuation's GitHub call is in
    /// flight, after the earlier request was stopped.
    struct FreshRequestDuringExecute<'a> {
        db: &'a WorkDb,
        id: &'a str,
        pr_url: &'a str,
    }

    #[async_trait]
    impl DirectMergeExecutor for FreshRequestDuringExecute<'_> {
        async fn execute(&self, _: &str) -> Result<DirectMergeSuccess> {
            self.db
                .stop_merge_conflict_resolution(self.id, "Stopped during merge")?;
            self.db.record_github_merge_intent(
                boss_work_db::GithubMergeIntentInsertInput::builder()
                    .work_item_id(self.id)
                    .pr_url(self.pr_url)
                    .head_sha("fresh-head")
                    .build(),
                "{}",
            )?;
            Ok(DirectMergeSuccess {
                head_sha: "resolved-head".into(),
            })
        }

        async fn disarm(&self, _: &str) -> Result<()> {
            anyhow::bail!("disarm must not run for a newer request")
        }
    }

    #[tokio::test]
    async fn a_newer_request_made_during_the_continuation_is_never_disarmed() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = boss_work_db::test_support::create_test_product_named(&db, "Newer request");
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
        db.start_merge_conflict_resolution(&task.id, &candidate.pr_url, "main")
            .unwrap();
        resume_after_conflict(
            &db,
            &NoopExecutionPublisher,
            &candidate,
            &FreshRequestDuringExecute {
                db: &db,
                id: &task.id,
                pr_url: &candidate.pr_url,
            },
        )
        .await;
        assert_eq!(
            db.get_active_github_merge_intent(&task.id, &candidate.pr_url)
                .unwrap()
                .unwrap()
                .head_sha,
            "fresh-head",
            "the newer request keeps its active intent"
        );
        assert!(!db.merge_disarm_pending(&task.id, &candidate.pr_url).unwrap());
        assert!(db.list_pending_merge_disarms().unwrap().is_empty());
        let counting = ResumeExecutor::new(false);
        disarm_stopped_merge(&db, &NoopExecutionPublisher, &candidate, &counting).await;
        assert_eq!(counting.disarms.load(Ordering::SeqCst), 0);
    }

    /// A fresh request lands while the disarm runs, then GitHub refuses to
    /// turn auto-merge back on for it.
    struct RearmFails<'a> {
        db: &'a WorkDb,
        id: &'a str,
        pr_url: &'a str,
    }

    #[async_trait]
    impl DirectMergeExecutor for RearmFails<'_> {
        async fn execute(&self, _: &str) -> Result<DirectMergeSuccess> {
            anyhow::bail!("gh pr merge failed: not allowed")
        }

        async fn disarm(&self, _: &str) -> Result<()> {
            self.db.record_github_merge_intent(
                boss_work_db::GithubMergeIntentInsertInput::builder()
                    .work_item_id(self.id)
                    .pr_url(self.pr_url)
                    .head_sha("fresh-head")
                    .build(),
                "{}",
            )?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_failed_rearm_after_a_disarm_retires_the_request_and_says_so() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = boss_work_db::test_support::create_test_product_named(&db, "Rearm");
        let task = boss_work_db::test_support::create_test_chore_manual(&db, product.id.clone(), "Merge");
        let pr_url = "https://github.com/example/repo/pull/42".to_owned();
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        db.record_github_merge_intent(
            boss_work_db::GithubMergeIntentInsertInput::builder()
                .work_item_id(&task.id)
                .pr_url(&pr_url)
                .head_sha("head")
                .build(),
            "{}",
        )
        .unwrap();
        db.mark_chore_blocked_merge_conflict(&task.id, &pr_url).unwrap();
        db.stop_merge_conflict_resolution(&task.id, "manual_abandon").unwrap();
        let candidate = PendingMergeCheck {
            work_item_id: task.id.clone(),
            product_id: product.id.clone(),
            pr_url: pr_url.clone(),
        };
        let executor = RearmFails {
            db: &db,
            id: &task.id,
            pr_url: &pr_url,
        };
        disarm_stopped_merge(&db, &NoopExecutionPublisher, &candidate, &executor).await;
        assert!(db.get_active_github_merge_intent(&task.id, &pr_url).unwrap().is_none());
        assert!(!db.merge_disarm_pending(&task.id, &pr_url).unwrap());
        let card = db
            .get_work_tree(&product.id)
            .unwrap()
            .chores
            .into_iter()
            .find(|t| t.id == task.id)
            .unwrap();
        assert_eq!(card.merge_queue_state, None, "the card must not claim a queued merge");
        let detail = card.merge_queue_detail.unwrap();
        assert!(detail.contains("turn auto-merge back on"), "{detail}");
        assert!(!detail.contains("turn off auto-merge"), "{detail}");
    }

    #[tokio::test]
    async fn auto_merge_that_is_already_off_completes_the_disarm() {
        struct AlreadyOff;
        #[async_trait]
        impl DirectMergeExecutor for AlreadyOff {
            async fn execute(&self, _: &str) -> Result<DirectMergeSuccess> {
                anyhow::bail!("unused")
            }
            async fn disarm(&self, _: &str) -> Result<()> {
                anyhow::bail!("gh pr merge --disable-auto failed: any wording at all")
            }
            async fn auto_merge_enabled(&self, _: &str) -> Result<bool> {
                Ok(false)
            }
        }
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = boss_work_db::test_support::create_test_product_named(&db, "Already off");
        let task = boss_work_db::test_support::create_test_chore_manual(&db, product.id.clone(), "Merge");
        let pr_url = "https://github.com/example/repo/pull/42".to_owned();
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        db.record_github_merge_intent(
            boss_work_db::GithubMergeIntentInsertInput::builder()
                .work_item_id(&task.id)
                .pr_url(&pr_url)
                .head_sha("head")
                .build(),
            "{}",
        )
        .unwrap();
        db.mark_chore_blocked_merge_conflict(&task.id, &pr_url).unwrap();
        db.stop_merge_conflict_resolution(&task.id, "manual_abandon").unwrap();
        let candidate = PendingMergeCheck {
            work_item_id: task.id.clone(),
            product_id: product.id.clone(),
            pr_url: pr_url.clone(),
        };
        disarm_stopped_merge(&db, &NoopExecutionPublisher, &candidate, &AlreadyOff).await;
        assert!(db.list_pending_merge_disarms().unwrap().is_empty());
    }

    /// While the older request's re-arm runs, that request is retired and a
    /// newer one becomes active; the older call then fails.
    struct RearmFailsAfterReplacement<'a> {
        db: &'a WorkDb,
        id: &'a str,
        pr_url: &'a str,
    }

    #[async_trait]
    impl DirectMergeExecutor for RearmFailsAfterReplacement<'_> {
        async fn execute(&self, _: &str) -> Result<DirectMergeSuccess> {
            self.db
                .retire_github_merge_intent(self.id, self.pr_url, "head_advanced")?;
            self.db.record_github_merge_intent(
                boss_work_db::GithubMergeIntentInsertInput::builder()
                    .work_item_id(self.id)
                    .pr_url(self.pr_url)
                    .head_sha("newest-head")
                    .build(),
                "{}",
            )?;
            anyhow::bail!("gh pr merge failed: late error")
        }

        async fn disarm(&self, _: &str) -> Result<()> {
            self.db.record_github_merge_intent(
                boss_work_db::GithubMergeIntentInsertInput::builder()
                    .work_item_id(self.id)
                    .pr_url(self.pr_url)
                    .head_sha("fresh-head")
                    .build(),
                "{}",
            )?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_late_rearm_failure_does_not_retire_a_newer_successful_request() {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = boss_work_db::test_support::create_test_product_named(&db, "Late rearm");
        let task = boss_work_db::test_support::create_test_chore_manual(&db, product.id.clone(), "Merge");
        let pr_url = "https://github.com/example/repo/pull/42".to_owned();
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        db.record_github_merge_intent(
            boss_work_db::GithubMergeIntentInsertInput::builder()
                .work_item_id(&task.id)
                .pr_url(&pr_url)
                .head_sha("head")
                .build(),
            "{}",
        )
        .unwrap();
        db.mark_chore_blocked_merge_conflict(&task.id, &pr_url).unwrap();
        db.stop_merge_conflict_resolution(&task.id, "manual_abandon").unwrap();
        let candidate = PendingMergeCheck {
            work_item_id: task.id.clone(),
            product_id: product.id.clone(),
            pr_url: pr_url.clone(),
        };
        let executor = RearmFailsAfterReplacement {
            db: &db,
            id: &task.id,
            pr_url: &pr_url,
        };
        disarm_stopped_merge(&db, &NoopExecutionPublisher, &candidate, &executor).await;
        assert_eq!(
            db.get_active_github_merge_intent(&task.id, &pr_url)
                .unwrap()
                .unwrap()
                .head_sha,
            "newest-head",
            "the newer request keeps its active intent"
        );
        let card = db
            .get_work_tree(&product.id)
            .unwrap()
            .chores
            .into_iter()
            .find(|t| t.id == task.id)
            .unwrap();
        assert_eq!(card.merge_queue_state.as_deref(), Some("queued"));
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
