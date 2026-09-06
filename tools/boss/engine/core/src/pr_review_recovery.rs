//! Auto-recovery sweep for `pr_review` executions that die without ever
//! finalizing — host failure, a cube-lease reap, or a worker crash.
//!
//! Incident (2026-07-03): the `pr_review` executions for PR
//! spinyfin/mono#1758 and a second PR were dispatched to a broken host,
//! never actually ran (no process; persistent `cube_lease_heartbeat`
//! errors), and were manually reaped by the coordinator. Because AI review
//! findings flow through the engine (not GitHub comments), a review
//! execution that dies silently means an open PR can reach merge with NO
//! review and nothing in the UI saying so. This sweep closes that gap:
//! it detects a `pr_review` execution that either reached a dead terminal
//! state or completed without a durable judgement (`gave_up`, or a missing
//! post-verdicts-table verdict — see
//! [`crate::work::WorkDb::list_dead_pr_review_candidates`]) and
//! re-enqueues a fresh review pass while the PR is still open.
//!
//! Complements [`crate::orphan_sweep`]: that sweep explicitly excludes
//! work items whose latest execution is a non-completed `pr_review` (see
//! `WorkDb::list_orphan_active_candidates`'s doc comment) so this module
//! owns them exclusively. `execution_kind_for_work_item` has no notion of
//! `pr_review` — it only derives the task-kind-based implementation kinds
//! — so if the generic sweep redispatched one of these items it would
//! wrongly spawn a fresh implementer on top of an already-open PR instead
//! of re-running the reviewer.
//!
//! Each pass:
//! 1. Lists dead-review candidates.
//! 2. Applies the same churn guard `orphan_sweep` uses: a work item whose
//!    recent terminal-execution count already hit the threshold is left
//!    alone for a human — a persistently broken host (like the incident's
//!    `anaplian`) must not spin a fresh doomed review forever.
//! 3. Re-fires the review via `WorkDb::request_pr_review`, which itself
//!    refuses when the PR has since merged or closed — this sweep just
//!    logs and skips those.
//! 4. Files an open attention item on the work item (kind
//!    `pr_review_died_without_findings`) so the gap between "review died,
//!    auto-refired" and "reviewed, clean" is visible on the kanban card
//!    and in `bossctl attentions` instead of looking identical to a clean
//!    pass.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{CreateAttentionItemInput, ReviewBatchMemberRole};

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::{
    DeadPrReviewCandidate, GhPrStateChecker, ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
    ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS, PrOpenState, PrStateChecker, RetryDeadReviewBatchMember, WorkDb,
    WorkItem,
};

/// Attention-item kind filed when a dead review is auto-refired — lets the
/// kanban surface and `bossctl attentions` distinguish "review died, was
/// auto-recovered" from a clean pass with no findings.
pub const PR_REVIEW_DIED_ATTENTION_KIND: &str = "pr_review_died_without_findings";

/// Counts from one pass of the sweep; logged at `info` when non-zero.
#[derive(Debug, Default)]
pub struct PrReviewRecoveryOutcome {
    pub refired: usize,
    pub churn_skipped: usize,
    pub pr_closed_skipped: usize,
    pub error_skipped: usize,
    /// Batches this pass terminal-failed (exhausted supervisor retry, or
    /// supervisor died after the PR head moved). Distinct from `refired`:
    /// nothing new is spawned, but the batch is no longer wedged in
    /// `supervising` with nothing raised.
    pub terminal_failed: usize,
}

impl crate::sweep_loop::SweepOutcome for PrReviewRecoveryOutcome {
    fn has_activity(&self) -> bool {
        self.refired > 0 || self.churn_skipped > 0 || self.pr_closed_skipped > 0 || self.terminal_failed > 0
    }

    fn log(&self) {
        tracing::info!(
            refired = self.refired,
            churn_skipped = self.churn_skipped,
            pr_closed_skipped = self.pr_closed_skipped,
            error_skipped = self.error_skipped,
            terminal_failed = self.terminal_failed,
            "pr_review recovery: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a review left dead by a previous engine
/// run (or a host outage discovered while the engine was down) is
/// recovered on boot without waiting for the first interval.
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
        |work_db, coordinator, dispatch_events| {
            Box::pin(run_one_pass(work_db, coordinator, dispatch_events, &GhPrStateChecker))
        },
    )
}

/// Run a single dead-review recovery pass. Returns a summary of what
/// happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because [`ExecutionCoordinator::kick`]
/// requires an `Arc<Self>` receiver — mirrors [`crate::orphan_sweep::run_one_pass`].
pub async fn run_one_pass(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    pr_checker: &dyn PrStateChecker,
) -> PrReviewRecoveryOutcome {
    let mut outcome = PrReviewRecoveryOutcome::default();

    // Batch leaves are independent read-only roles, so they must not pass
    // through the legacy one-row-per-work-item candidate query below. Recover
    // each role exactly once from its durable member policy; this preserves
    // the assigned driver/model and never turns one failed leaf into a second
    // single-reviewer pass.
    let batch_candidates = match work_db.list_dead_review_batch_member_candidates() {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(
                ?error,
                "pr_review recovery: failed to list dead batch members; skipping batch recovery"
            );
            Vec::new()
        }
    };
    for candidate in batch_candidates {
        let DeadPrReviewCandidate {
            work_item_id,
            execution_id: dead_execution_id,
            execution_status: dead_status,
        } = candidate;

        // Mirror the legacy loop below: refuse to refire a leaf against a PR
        // that has since merged or closed. The candidate query's task-status
        // filter only excludes `done`/`archived`, which misses both a PR
        // closed without merging and the window between a merge landing and
        // the merge poller moving the task to `done` — without this check a
        // leaf would spawn a reviewer (and burn a review-pool slot and a
        // cube lease) for a PR that no longer needs one.
        let pr_url = match work_db.get_work_item(&work_item_id) {
            Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => task.pr_url,
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    ?error,
                    "pr_review recovery: failed to look up work item for batch leaf; skipping",
                );
                outcome.error_skipped += 1;
                continue;
            }
        };
        let pr_url = match pr_url.filter(|url| !url.is_empty()) {
            Some(pr_url) => pr_url,
            None => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    "pr_review recovery: batch leaf's work item has no pr_url; skipping",
                );
                outcome.error_skipped += 1;
                continue;
            }
        };
        let inspect = match pr_checker.inspect(&pr_url) {
            Ok(inspect) => inspect,
            Err(error) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    ?error,
                    "pr_review recovery: failed to check PR state for batch member; skipping",
                );
                outcome.error_skipped += 1;
                continue;
            }
        };
        match inspect.open_state {
            PrOpenState::Open => {}
            PrOpenState::Merged | PrOpenState::ClosedUnmerged => {
                match work_db.fail_review_batch_for_closed_pr(&dead_execution_id) {
                    Ok(true) => {
                        tracing::warn!(
                            work_item_id = %work_item_id,
                            dead_execution_id = %dead_execution_id,
                            "pr_review recovery: PR closed while its dead supervisor awaited recovery; failed the batch",
                        );
                        outcome.terminal_failed += 1;
                    }
                    Ok(false) => {
                        tracing::info!(
                            work_item_id = %work_item_id,
                            dead_execution_id = %dead_execution_id,
                            "pr_review recovery: PR is no longer open; skipping batch member recovery",
                        );
                        outcome.pr_closed_skipped += 1;
                    }
                    Err(error) => {
                        tracing::warn!(
                            work_item_id = %work_item_id,
                            dead_execution_id = %dead_execution_id,
                            ?error,
                            "pr_review recovery: failed to settle supervisor batch for closed PR",
                        );
                        outcome.error_skipped += 1;
                    }
                }
                continue;
            }
        }

        // A retried supervisor collates the existing leaf reports for the
        // batch's frozen target SHA. If the PR head has moved, those reports
        // are stale; fail the batch rather than consolidating them.
        match work_db.review_batch_member_for_execution(&dead_execution_id) {
            Ok(Some(member)) if member.role == ReviewBatchMemberRole::Supervisor => {
                match work_db.review_batch(&member.batch_id) {
                    Ok(Some(batch)) => {
                        if let Some(live_sha) = inspect.head_sha.as_deref()
                            && live_sha != batch.target_sha
                        {
                            match work_db.fail_review_batch_for_moved_head(&dead_execution_id, live_sha) {
                                Ok(true) => {
                                    tracing::warn!(
                                        work_item_id = %work_item_id,
                                        dead_execution_id = %dead_execution_id,
                                        batch_id = %batch.id,
                                        batch_target_sha = %batch.target_sha,
                                        live_head_sha = %live_sha,
                                        "pr_review recovery: supervisor died after PR head moved; failed the batch",
                                    );
                                    outcome.terminal_failed += 1;
                                }
                                Ok(false) => {
                                    tracing::info!(
                                        work_item_id = %work_item_id,
                                        dead_execution_id = %dead_execution_id,
                                        "pr_review recovery: supervisor head-move fail was a no-op",
                                    );
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        work_item_id = %work_item_id,
                                        dead_execution_id = %dead_execution_id,
                                        ?error,
                                        "pr_review recovery: failed to fail batch after PR head moved",
                                    );
                                    outcome.error_skipped += 1;
                                }
                            }
                            continue;
                        }
                    }
                    Ok(None) => {
                        tracing::warn!(
                            work_item_id = %work_item_id,
                            dead_execution_id = %dead_execution_id,
                            batch_id = %member.batch_id,
                            "pr_review recovery: supervisor member has no persisted batch; skipping",
                        );
                        outcome.error_skipped += 1;
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(
                            work_item_id = %work_item_id,
                            dead_execution_id = %dead_execution_id,
                            ?error,
                            "pr_review recovery: failed to load batch for supervisor SHA check; skipping",
                        );
                        outcome.error_skipped += 1;
                        continue;
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    ?error,
                    "pr_review recovery: failed to identify batch membership; skipping",
                );
                outcome.error_skipped += 1;
                continue;
            }
        }

        match work_db.retry_dead_review_batch_member(&dead_execution_id) {
            Ok(RetryDeadReviewBatchMember::Retried(retry)) => {
                file_dead_review_attention(work_db, &work_item_id, &dead_execution_id, dead_status.as_str());
                dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PrReviewDeadRecovery, Outcome::Ok, &retry.id)
                            .with_work_item(&work_item_id)
                            .with_details(serde_json::json!({
                                "dead_execution_id": dead_execution_id,
                                "dead_execution_status": dead_status.as_str(),
                                "recovery_mode": "review_batch_member",
                            })),
                    )
                    .await;
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    retry_execution_id = %retry.id,
                    "pr_review recovery: refired one role-scoped batch member",
                );
                coordinator.kick();
                outcome.refired += 1;
            }
            Ok(RetryDeadReviewBatchMember::BatchFailed) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    "pr_review recovery: batch member exhausted its retry; batch failed with attention",
                );
                outcome.terminal_failed += 1;
            }
            Ok(RetryDeadReviewBatchMember::NotRetried) => {
                tracing::info!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    "pr_review recovery: batch member already exhausted its one retry",
                );
            }
            Err(error) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    ?error,
                    "pr_review recovery: failed to recover batch member",
                );
                outcome.error_skipped += 1;
            }
        }
    }

    // Post-merge batch members are a different topology from the pre-merge
    // loop above: their batch exists precisely *because* the PR already
    // merged, so none of that loop's "is the PR still open?" / "did the PR
    // head move past the frozen target?" checks apply — a post-merge
    // target's SHA is a landed commit, which by definition never moves.
    // Retry (or terminal-fail on an exhausted attempt) is therefore a direct
    // call into the same role-scoped mechanics the pre-merge loop uses.
    let post_merge_candidates = match work_db.list_dead_post_merge_review_batch_member_candidates() {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(
                ?error,
                "pr_review recovery: failed to list dead post-merge batch members; skipping post-merge recovery"
            );
            Vec::new()
        }
    };
    for candidate in post_merge_candidates {
        let DeadPrReviewCandidate {
            work_item_id,
            execution_id: dead_execution_id,
            execution_status: dead_status,
        } = candidate;
        match work_db.retry_dead_review_batch_member(&dead_execution_id) {
            Ok(RetryDeadReviewBatchMember::Retried(retry)) => {
                file_dead_review_attention(work_db, &work_item_id, &dead_execution_id, dead_status.as_str());
                dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PrReviewDeadRecovery, Outcome::Ok, &retry.id)
                            .with_work_item(&work_item_id)
                            .with_details(serde_json::json!({
                                "dead_execution_id": dead_execution_id,
                                "dead_execution_status": dead_status.as_str(),
                                "recovery_mode": "post_merge_review_batch_member",
                            })),
                    )
                    .await;
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    retry_execution_id = %retry.id,
                    "pr_review recovery: refired one dead post-merge reviewer (retry 1 of 1)",
                );
                coordinator.kick();
                outcome.refired += 1;
            }
            Ok(RetryDeadReviewBatchMember::BatchFailed) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    "pr_review recovery: post-merge reviewer exhausted its retry; batch failed with attention",
                );
                outcome.terminal_failed += 1;
            }
            Ok(RetryDeadReviewBatchMember::NotRetried) => {
                tracing::info!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    "pr_review recovery: post-merge reviewer already exhausted its one retry",
                );
            }
            Err(error) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    ?error,
                    "pr_review recovery: failed to recover post-merge reviewer batch member",
                );
                outcome.error_skipped += 1;
            }
        }
    }

    let candidates = match work_db.list_dead_pr_review_candidates() {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(?err, "pr_review recovery: failed to list candidates; skipping pass");
            return outcome;
        }
    };

    let now_epoch_secs: i64 = boss_engine_utils::epoch_time::now_epoch_secs();
    let churn_cutoff = now_epoch_secs - ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS;

    for candidate in candidates {
        let DeadPrReviewCandidate {
            work_item_id,
            execution_id: dead_execution_id,
            execution_status: dead_status,
        } = candidate;

        match work_db.review_batch_member_for_execution(&dead_execution_id) {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    ?error,
                    "pr_review recovery: failed to identify batch membership; skipping legacy recovery",
                );
                outcome.error_skipped += 1;
                continue;
            }
        }

        // Churn guard: a work item whose *reviews* keep failing to produce
        // an informative result is almost certainly hitting something
        // structural (a persistently broken host, like the incident's
        // `anaplian`) rather than a one-off blip. Re-firing forever would
        // just burn another unproductive review
        // every pass; leave it for a human instead. Reuses the same
        // threshold/window `orphan_sweep` uses — both guards exist to stop
        // an unproductive redispatch loop, so one operator-tunable
        // constant covers both.
        //
        // Scoped to `kind = pr_review` (unlike `orphan_sweep`'s unscoped
        // count): a work item routinely accumulates terminal
        // `chore_implementation`/`revision_implementation` retries in the
        // same trailing hour a review is first dispatched in — they happen
        // back-to-back as part of the same work session and say nothing
        // about whether the review itself is healthy. Counting them against
        // the review's churn budget let a single transient review failure
        // trip the guard immediately, parking the item and leaving the PR
        // permanently unreviewed instead of getting the retry this guard is
        // supposed to allow.
        let recent_terminal = match work_db.count_recent_uninformative_pr_review_executions(&work_item_id, churn_cutoff)
        {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "pr_review recovery: failed to count recent terminal executions; skipping item",
                );
                outcome.error_skipped += 1;
                continue;
            }
        };
        if recent_terminal >= ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            tracing::warn!(
                work_item_id = %work_item_id,
                dead_execution_id = %dead_execution_id,
                recent_terminal,
                threshold = ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
                window_secs = ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS,
                "pr_review recovery: churn guard tripped; not auto-refiring — human attention required",
            );
            let failing_ids = work_db
                .list_recent_uninformative_pr_review_execution_ids(&work_item_id, churn_cutoff)
                .unwrap_or_default();
            work_db.file_churn_guard_parked_attention(
                &work_item_id,
                "pr_review_recovery",
                recent_terminal,
                &failing_ids,
                "unproductive pr_review executions",
            );
            outcome.churn_skipped += 1;
            continue;
        }

        match work_db.request_pr_review(&work_item_id, pr_checker) {
            Ok(execution) => {
                file_dead_review_attention(work_db, &work_item_id, &dead_execution_id, dead_status.as_str());

                dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PrReviewDeadRecovery, Outcome::Ok, &execution.id)
                            .with_work_item(&work_item_id)
                            .with_details(serde_json::json!({
                                "dead_execution_id": dead_execution_id,
                                "dead_execution_status": dead_status.as_str(),
                            })),
                    )
                    .await;

                tracing::warn!(
                    work_item_id = %work_item_id,
                    dead_execution_id = %dead_execution_id,
                    dead_execution_status = %dead_status,
                    new_execution_id = %execution.id,
                    "pr_review recovery: auto-refired a review that died without producing findings",
                );

                coordinator.kick();
                outcome.refired += 1;
            }
            Err(err) => {
                // `request_pr_review` refuses (rather than silently
                // no-op-ing) when the PR has since merged or closed —
                // that is an expected, non-error outcome here: the item
                // moved on before this sweep got to it.
                let message = err.to_string();
                if message.contains("already merged") || message.contains("is closed") {
                    tracing::info!(
                        work_item_id = %work_item_id,
                        dead_execution_id = %dead_execution_id,
                        "pr_review recovery: PR is no longer open; nothing to review",
                    );
                    outcome.pr_closed_skipped += 1;
                } else {
                    tracing::warn!(
                        work_item_id = %work_item_id,
                        dead_execution_id = %dead_execution_id,
                        error = %message,
                        "pr_review recovery: failed to re-fire review; skipping item",
                    );
                    outcome.error_skipped += 1;
                }
            }
        }
    }

    outcome
}

/// File an open attention item recording that this work item's review died
/// without producing findings and was auto-refired. Best-effort: a failure
/// here is logged and swallowed — it must never abort the re-fire itself,
/// which already succeeded by the time this is called.
fn file_dead_review_attention(work_db: &WorkDb, work_item_id: &str, dead_execution_id: &str, dead_status: &str) {
    let body = format!(
        "The automated reviewer for this PR (execution `{dead_execution_id}`) reached a terminal \
         `{dead_status}` state without ever producing a `ReviewResult` — the review died before \
         finishing (host failure, a cube-lease reap, or a crash), not because it found the PR \
         clean. The engine has automatically re-enqueued a fresh review pass. This item is \
         distinct from \"reviewed, no findings\" — dismiss it once the re-fired review completes."
    );
    if let Err(err) = work_db.create_attention_item(CreateAttentionItemInput {
        execution_id: None,
        work_item_id: Some(work_item_id.to_owned()),
        kind: PR_REVIEW_DIED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Automated review died without findings — auto-refired".to_owned(),
        body_markdown: body,
        resolved_at: None,
    }) {
        tracing::warn!(
            work_item_id = %work_item_id,
            ?err,
            "pr_review recovery: failed to file dead-review attention item",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use boss_protocol::{
        ExecutionKind, RequestExecutionInput, ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase,
        ReviewBatchStatus, ReviewClassification, ReviewLanguageBucket, ReviewProfile,
    };

    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::test_support::*;
    use crate::work::{
        CreateChoreInput, CreateExecutionInput, ExecutionStatus, FakePrStateChecker, PrOpenState,
        ReviewBatchCreateInput, ReviewBatchMemberCreateInput, WorkDb, WorkItemPatch,
    };

    // `NoopCube` and `NoopRunner` come from `crate::test_support::*`.

    /// Create an active chore with a bound `pr_url` and a dead (orphaned)
    /// `pr_review` execution — the exact shape `list_dead_pr_review_candidates`
    /// targets.
    fn create_chore_with_dead_review(db: &WorkDb, pr_url: &str) -> (String, String) {
        let product_id = create_test_product_with_repo(db, "test-product", Some("https://github.com/test/repo")).id;
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("test chore")
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                pr_url: Some(pr_url.to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET kind = 'pr_review', status = 'orphaned' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();
        }
        (chore.id, execution.id)
    }

    #[tokio::test]
    async fn refires_dead_review_and_files_attention() {
        let (_dir, db) = open_db();
        let (work_item_id, dead_execution_id) =
            create_chore_with_dead_review(&db, "https://github.com/test/repo/pull/1");

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(outcome.refired, 1, "dead review should have been auto-refired");

        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions.iter().any(|e| e.kind == ExecutionKind::PrReview
                && e.status == ExecutionStatus::Ready
                && e.id != dead_execution_id),
            "expected a fresh ready pr_review execution distinct from the dead one"
        );

        let attentions = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert!(
            attentions.iter().any(|a| a.kind == PR_REVIEW_DIED_ATTENTION_KIND),
            "expected a pr_review_died_without_findings attention item; got: {attentions:?}"
        );

        let events = sink.events().await;
        assert!(
            events.iter().any(|e| e.stage == "pr_review_dead_recovery"),
            "expected a pr_review_dead_recovery dispatch event"
        );
    }

    #[tokio::test]
    async fn skips_when_pr_already_merged() {
        let (_dir, db) = open_db();
        let (work_item_id, _dead_execution_id) =
            create_chore_with_dead_review(&db, "https://github.com/test/repo/pull/2");

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Merged);

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(outcome.refired, 0);
        assert_eq!(outcome.pr_closed_skipped, 1);

        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            !executions
                .iter()
                .any(|e| e.kind == ExecutionKind::PrReview && e.status == ExecutionStatus::Ready),
            "no fresh pr_review execution should be created once the PR is merged"
        );
    }

    #[tokio::test]
    async fn churn_guard_skips_repeatedly_dying_review() {
        let (_dir, db) = open_db();
        let (work_item_id, _dead_execution_id) =
            create_chore_with_dead_review(&db, "https://github.com/test/repo/pull/3");

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            db.insert_terminal_execution_for_test(&work_item_id, "pr_review", "orphaned", now_epoch - i)
                .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(outcome.churn_skipped, 1, "churn guard should have fired");
        assert_eq!(outcome.refired, 0);
        assert!(sink.events().await.is_empty(), "no event on churn skip");

        // Same operator-visible signal as orphan_sweep's churn guard: a
        // `churn_guard_parked` attention item, not just a trace WARN.
        let attentions = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert!(
            attentions
                .iter()
                .any(|a| a.kind == crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND && a.status == "open"),
            "expected an open churn_guard_parked attention item; got: {attentions:?}"
        );

        // Bypassing the guard (`bossctl work start`) resolves it immediately.
        db.request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
            |_| false,
        )
        .unwrap();
        let attentions_after = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert!(
            attentions_after
                .iter()
                .filter(|a| a.kind == crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND)
                .all(|a| a.status == "resolved"),
            "churn_guard_parked attention should auto-resolve on the next dispatch attempt"
        );
    }

    /// Regression: `list_dead_pr_review_candidates` treats `cancelled`
    /// pr_review executions as dead-review candidates, so the churn guard
    /// must count them too — otherwise a review that repeatedly ends
    /// `cancelled` never accumulates toward the threshold and gets
    /// refired by every sweep pass with the guard never tripping.
    #[tokio::test]
    async fn churn_guard_counts_cancelled_reviews() {
        let (_dir, db) = open_db();
        let (work_item_id, _dead_execution_id) =
            create_chore_with_dead_review(&db, "https://github.com/test/repo/pull/9");

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            db.insert_terminal_execution_for_test(&work_item_id, "pr_review", "cancelled", now_epoch - i)
                .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(
            outcome.churn_skipped, 1,
            "cancelled pr_review executions must count toward the churn guard"
        );
        assert_eq!(outcome.refired, 0);
    }

    #[tokio::test]
    async fn churn_guard_park_auto_clears_via_recovery_sweep_once_window_drains() {
        // Regression test for the real auto-recovery path: the sweep must
        // clear its own `churn_guard_parked` attention when it successfully
        // re-fires a review, not just when an operator bypasses the guard
        // via `bossctl work start`. Unlike
        // `churn_guard_skips_repeatedly_dying_review`, this drives the
        // clear through `run_one_pass` / `request_pr_review` — the code
        // path `pr_review_recovery` actually takes when it heals.
        let (_dir, db) = open_db();
        let (work_item_id, _dead_execution_id) =
            create_chore_with_dead_review(&db, "https://github.com/test/repo/pull/5");

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            db.insert_terminal_execution_for_test(&work_item_id, "pr_review", "orphaned", now_epoch - i)
                .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;
        assert_eq!(outcome.churn_skipped, 1, "churn guard should have fired");
        assert_eq!(outcome.refired, 0);

        let attentions = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert!(
            attentions
                .iter()
                .any(|a| a.kind == crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND && a.status == "open"),
            "expected an open churn_guard_parked attention item; got: {attentions:?}"
        );

        // Simulate the trailing window draining: age the terminal
        // executions out of `ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS`.
        db.backdate_terminal_executions_for_test(
            &work_item_id,
            now_epoch - ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS - 1,
        )
        .unwrap();

        let outcome_after_drain = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;
        assert_eq!(
            outcome_after_drain.refired, 1,
            "recovery sweep should auto-refire once the churn window drains"
        );
        assert_eq!(outcome_after_drain.churn_skipped, 0);

        let attentions_after = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert!(
            attentions_after
                .iter()
                .filter(|a| a.kind == crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND)
                .all(|a| a.status == "resolved"),
            "churn_guard_parked attention should auto-resolve once the recovery sweep re-fires the review; \
             got: {attentions_after:?}"
        );
    }

    /// Regression: unrelated `chore_implementation` churn (the normal
    /// back-and-forth of a work item reaching a PR at all) must NOT count
    /// against the `pr_review` churn guard. Before the kind-scoped fix,
    /// `count_recent_terminal_executions` counted terminal executions of
    /// ANY kind, so a work item that had already burned through
    /// `ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD - 1` failed implementation
    /// attempts in the trailing window would trip the guard on the review's
    /// very FIRST dead attempt — parking the item and leaving the PR
    /// permanently unreviewed instead of getting the retry the guard is
    /// supposed to allow.
    #[tokio::test]
    async fn unrelated_implementation_churn_does_not_trip_review_churn_guard() {
        let (_dir, db) = open_db();
        let (work_item_id, _dead_execution_id) =
            create_chore_with_dead_review(&db, "https://github.com/test/repo/pull/6");

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        // Enough unrelated chore_implementation churn, on its own, to have
        // tripped the old unscoped guard (threshold - 1 plus the one dead
        // pr_review from create_chore_with_dead_review == threshold).
        for i in 0..(ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD - 1) {
            db.insert_terminal_execution_for_test(&work_item_id, "chore_implementation", "failed", now_epoch - i)
                .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(
            outcome.churn_skipped, 0,
            "unrelated chore_implementation churn must not count toward the pr_review churn guard"
        );
        assert_eq!(
            outcome.refired, 1,
            "the review's first dead attempt must still be auto-refired"
        );

        let attentions = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert!(
            attentions
                .iter()
                .all(|a| a.kind != crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND),
            "no churn_guard_parked attention should be filed; got: {attentions:?}"
        );
    }

    #[tokio::test]
    async fn no_candidates_when_review_completed_normally() {
        let (_dir, db) = open_db();
        let product_id = create_test_product_with_repo(&db, "test-product", Some("https://github.com/test/repo")).id;
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("test chore")
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some("https://github.com/test/repo/pull/4".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = db
            .create_execution(
                boss_protocol::CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "review-worker",
            "review-host",
            "review-lease",
            "review-workspace",
            "/tmp/review-workspace",
        )
        .unwrap();
        db.record_worker_pr_completion(
            &execution.id,
            "https://github.com/test/repo/pull/4",
            None,
            None,
            crate::work::WorkerPrCompletionTarget::InReview,
            Some(crate::work::ReviewVerdictInput {
                head_sha: Some("reviewed-head".to_owned()),
                findings_count: 0,
                revision_warranted: false,
                gate_outcome: crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
            }),
        )
        .unwrap();

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(
            outcome.refired, 0,
            "a normally-completed review must not be treated as dead"
        );
        assert!(sink.events().await.is_empty());
    }

    fn in_review_chore_with_pr(db: &WorkDb, pr_url: &str) -> String {
        let product_id = create_test_product_with_repo(db, "test-product", Some("https://github.com/test/repo")).id;
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("test chore")
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr_url.to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        chore.id
    }

    fn completed_pr_review_with_verdict(
        db: &WorkDb,
        work_item_id: &str,
        gate_outcome: &'static str,
        created_at: Option<&str>,
    ) -> String {
        let execution = db
            .create_execution(
                boss_protocol::CreateExecutionInput::builder()
                    .work_item_id(work_item_id.to_owned())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        if let Some(created_at) = created_at {
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
                    rusqlite::params![execution.id, created_at],
                )
                .unwrap();
        }
        crate::work::WorkDb::insert_review_verdict_in_tx(
            &db.connect().unwrap(),
            &execution.id,
            work_item_id,
            &crate::work::ReviewVerdictInput {
                head_sha: Some("reviewed-head".to_owned()),
                findings_count: 0,
                revision_warranted: gate_outcome == crate::work::REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD,
                gate_outcome,
            },
        )
        .unwrap();
        execution.id
    }

    /// `dropped_duplicate_head` produced a ReviewResult; the earlier covering
    /// pass already reviewed the head. Recovery must not treat it as dead or
    /// yank the card out of Review.
    #[tokio::test]
    async fn dropped_duplicate_head_is_not_a_dead_review() {
        let (_dir, db) = open_db();
        let work_item_id = in_review_chore_with_pr(&db, "https://github.com/test/repo/pull/11");
        completed_pr_review_with_verdict(
            &db,
            &work_item_id,
            crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
            Some("10"),
        );
        let dropped_id = completed_pr_review_with_verdict(
            &db,
            &work_item_id,
            crate::work::REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD,
            None,
        );

        assert!(
            !db.list_dead_pr_review_candidates()
                .unwrap()
                .iter()
                .any(|candidate| candidate.work_item_id == work_item_id),
            "dropped_duplicate_head must not be a dead-review candidate"
        );
        assert_eq!(
            db.count_recent_uninformative_pr_review_executions(&work_item_id, 0)
                .unwrap(),
            0,
            "dropped_duplicate_head must not count toward the uninformative churn budget"
        );

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let outcome = run_one_pass(db.as_ref(), coordinator, sink.as_ref(), &checker).await;

        assert_eq!(outcome.refired, 0);
        let task = match db.get_work_item(&work_item_id).unwrap() {
            crate::work::WorkItem::Chore(task) | crate::work::WorkItem::Task(task) => task,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(task.status, crate::work::TaskStatus::InReview);
        assert!(
            !db.list_executions(Some(&work_item_id))
                .unwrap()
                .iter()
                .any(|execution| execution.kind == ExecutionKind::PrReview
                    && execution.status == ExecutionStatus::Ready
                    && execution.id != dropped_id)
        );
    }

    /// Completed pr_review rows that predate `pr_review_verdicts` have no
    /// verdict row; they were a finished review under the old completed-
    /// status-only signal and must not become dead candidates on upgrade.
    #[tokio::test]
    async fn pre_verdicts_table_completed_review_is_not_dead() {
        let (_dir, db) = open_db();
        let work_item_id = in_review_chore_with_pr(&db, "https://github.com/test/repo/pull/12");
        let execution = db
            .create_execution(
                boss_protocol::CreateExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET created_at = '1' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();

        assert!(
            !db.list_dead_pr_review_candidates()
                .unwrap()
                .iter()
                .any(|candidate| candidate.work_item_id == work_item_id),
            "a completed pr_review created before pr_review_verdicts existed must not be dead"
        );

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);
        let outcome = run_one_pass(db.as_ref(), coordinator, sink.as_ref(), &checker).await;
        assert_eq!(outcome.refired, 0);
        let task = match db.get_work_item(&work_item_id).unwrap() {
            crate::work::WorkItem::Chore(task) | crate::work::WorkItem::Task(task) => task,
            other => panic!("expected chore, got {other:?}"),
        };
        assert_eq!(task.status, crate::work::TaskStatus::InReview);
    }

    /// A completed pr_review created after the verdicts table existed, with
    /// no verdict row, is a genuine missing judgement and stays a dead
    /// candidate.
    #[test]
    fn post_verdicts_completed_without_verdict_is_dead() {
        let (_dir, db) = open_db();
        let work_item_id = in_review_chore_with_pr(&db, "https://github.com/test/repo/pull/13");
        let stamp: i64 = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = ?1",
                rusqlite::params!["pr_review_verdicts_since"],
                |row| row.get(0),
            )
            .unwrap();
        let execution = db
            .create_execution(
                boss_protocol::CreateExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
                rusqlite::params![execution.id, (stamp + 10).to_string()],
            )
            .unwrap();

        assert!(
            db.list_dead_pr_review_candidates()
                .unwrap()
                .iter()
                .any(|candidate| candidate.work_item_id == work_item_id && candidate.execution_id == execution.id),
            "a completed pr_review created after pr_review_verdicts, with no verdict, is dead"
        );
    }

    fn create_chore_with_dead_supervisor(db: &WorkDb, pr_url: &str, target_sha: &str) -> (String, String, String) {
        let work_item_id = in_review_chore_with_pr(db, pr_url);
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let classification = ReviewClassification::builder()
            .changed_files(vec!["src/lib.rs".to_owned()])
            .complexity_flags(vec![])
            .has_production_code(true)
            .metadata_missing(vec![])
            .production_languages(vec![ReviewLanguageBucket::Rust])
            .profile(ReviewProfile::Light)
            .subsystem_buckets(vec!["src".to_owned()])
            .build();
        let (batch, _) = db
            .create_review_batch(
                ReviewBatchCreateInput::builder()
                    .cycle_root_id(work_item_id.clone())
                    .base_sha("base-sha")
                    .classification(classification)
                    .phase(ReviewBatchPhase::PreMerge)
                    .pr_number(1)
                    .pr_url(pr_url)
                    .target_sha(target_sha)
                    .build(),
                &[ReviewBatchMemberCreateInput::builder()
                    .attempt(1)
                    .provider_effort("medium")
                    .requested_driver("claude")
                    .resolved_model("test-model")
                    .role(ReviewBatchMemberRole::Supervisor)
                    .status(ReviewBatchMemberStatus::Pending)
                    .execution_id(execution.id.clone())
                    .build()],
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_batches SET status = 'supervising' WHERE id = ?1",
                rusqlite::params![batch.id],
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();
        (work_item_id, execution.id, batch.id)
    }

    #[tokio::test]
    async fn refires_dead_supervisor_when_head_sha_matches() {
        let (_dir, db) = open_db();
        let (work_item_id, dead_execution_id, batch_id) =
            create_chore_with_dead_supervisor(&db, "https://github.com/test/repo/pull/21", "head-sha");

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head-sha");

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(outcome.refired, 1, "dead supervisor should have been retried once");
        assert_eq!(outcome.terminal_failed, 0);
        assert_eq!(
            db.review_batch(&batch_id).unwrap().unwrap().status,
            ReviewBatchStatus::Supervising
        );
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions.iter().any(|e| e.kind == ExecutionKind::PrReview
                && e.status == ExecutionStatus::Ready
                && e.id != dead_execution_id),
            "expected a fresh ready supervisor execution distinct from the dead one"
        );
        let members = db.review_batch_members(&batch_id).unwrap();
        assert_eq!(
            members
                .iter()
                .filter(|m| m.role == ReviewBatchMemberRole::Supervisor)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn refires_dead_supervisor_when_head_sha_is_unknown() {
        let (_dir, db) = open_db();
        let (_work_item_id, _dead_execution_id, batch_id) =
            create_chore_with_dead_supervisor(&db, "https://github.com/test/repo/pull/24", "head-sha");

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);

        let outcome = run_one_pass(db.as_ref(), coordinator, sink.as_ref(), &checker).await;

        assert_eq!(outcome.refired, 1, "an unknown head SHA must not be treated as a move");
        assert_eq!(outcome.terminal_failed, 0);
        assert_eq!(
            db.review_batch(&batch_id).unwrap().unwrap().status,
            ReviewBatchStatus::Supervising
        );
    }

    #[tokio::test]
    async fn fails_dead_supervisor_batch_when_pr_head_has_moved() {
        let (_dir, db) = open_db();
        let (work_item_id, dead_execution_id, batch_id) =
            create_chore_with_dead_supervisor(&db, "https://github.com/test/repo/pull/22", "head-sha");

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open).with_head_sha("new-head");

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(outcome.refired, 0, "must not retry a supervisor against a moved head");
        assert_eq!(outcome.terminal_failed, 1);
        let failed = db.review_batch(&batch_id).unwrap().unwrap();
        assert_eq!(failed.status, ReviewBatchStatus::Failed);
        assert!(
            !db.list_executions(Some(&work_item_id))
                .unwrap()
                .iter()
                .any(|e| e.kind == ExecutionKind::PrReview
                    && e.status == ExecutionStatus::Ready
                    && e.id != dead_execution_id),
            "moved-head failure must not spawn a retry execution"
        );
        let attentions = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert!(
            attentions
                .iter()
                .any(|a| a.kind == "pr_review_quorum_failed" && a.title.to_lowercase().contains("head moved")),
            "expected a pr_review_quorum_failed attention; got: {attentions:?}"
        );
    }

    #[tokio::test]
    async fn fails_dead_supervisor_batch_when_pr_already_merged() {
        let (_dir, db) = open_db();
        let (_work_item_id, _dead_execution_id, batch_id) =
            create_chore_with_dead_supervisor(&db, "https://github.com/test/repo/pull/23", "head-sha");

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Merged).with_head_sha("head-sha");

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(outcome.refired, 0);
        assert_eq!(outcome.pr_closed_skipped, 0);
        assert_eq!(outcome.terminal_failed, 1);
        assert_eq!(
            db.review_batch(&batch_id).unwrap().unwrap().status,
            ReviewBatchStatus::Failed,
            "a merged PR must settle a dead supervisor batch rather than leaving it supervising"
        );
    }
}
