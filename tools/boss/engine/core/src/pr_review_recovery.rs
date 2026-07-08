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
//! it detects a `pr_review` execution that reached a terminal state
//! WITHOUT ever producing a `ReviewResult` (see
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boss_protocol::CreateAttentionItemInput;

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::{
    DeadPrReviewCandidate, GhPrStateChecker, ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
    ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS, PrStateChecker, WorkDb,
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
}

impl crate::sweep_loop::SweepOutcome for PrReviewRecoveryOutcome {
    fn has_activity(&self) -> bool {
        self.refired > 0 || self.churn_skipped > 0 || self.pr_closed_skipped > 0
    }

    fn log(&self) {
        tracing::info!(
            refired = self.refired,
            churn_skipped = self.churn_skipped,
            pr_closed_skipped = self.pr_closed_skipped,
            error_skipped = self.error_skipped,
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
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        async move {
            run_one_pass(
                work_db.as_ref(),
                coordinator,
                dispatch_events.as_ref(),
                &GhPrStateChecker,
            )
            .await
        }
    })
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

    let candidates = match work_db.list_dead_pr_review_candidates() {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(?err, "pr_review recovery: failed to list candidates; skipping pass");
            return outcome;
        }
    };

    let now_epoch_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let churn_cutoff = now_epoch_secs - ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS;

    for candidate in candidates {
        let DeadPrReviewCandidate {
            work_item_id,
            execution_id: dead_execution_id,
            execution_status: dead_status,
        } = candidate;

        // Churn guard: a work item that keeps producing terminal
        // executions (of any kind, in the trailing window) is almost
        // certainly hitting something structural (a persistently broken
        // host, like the incident's `anaplian`) rather than a one-off
        // blip. Re-firing forever would just burn another doomed review
        // every pass; leave it for a human instead. Reuses the same
        // threshold/window `orphan_sweep` uses — both guards exist to stop
        // an unproductive redispatch loop, so one operator-tunable
        // constant covers both.
        let recent_terminal = match work_db.count_recent_terminal_executions(&work_item_id, churn_cutoff) {
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

    use anyhow::Result;
    use async_trait::async_trait;
    use boss_protocol::{ExecutionKind, RequestExecutionInput};
    use tempfile::TempDir;

    use super::*;
    use crate::coordinator::{ExecutionCoordinator, WorkerPool};
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::runner::{ExecutionRunner, RunOutcome};
    use crate::test_support::*;
    use crate::work::{CreateChoreInput, ExecutionStatus, FakePrStateChecker, PrOpenState, WorkDb, WorkItemPatch};
    use boss_protocol::WorkExecution;

    // `NoopCube` comes from `crate::test_support::*`.

    struct NoopRunner;

    #[async_trait]
    impl ExecutionRunner for NoopRunner {
        async fn run_execution(
            &self,
            _worker_id: &str,
            _execution: &WorkExecution,
            _work_item: &crate::work::WorkItem,
            _workspace_path: &std::path::Path,
            _cube_change_id: Option<&str>,
        ) -> Result<RunOutcome> {
            unimplemented!("pr_review_recovery tests don't run executions")
        }
    }

    fn open_db() -> (TempDir, WorkDb) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    fn make_coordinator(db: Arc<WorkDb>) -> Arc<ExecutionCoordinator> {
        Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(1),
            Arc::new(NoopCube),
            Arc::new(NoopRunner),
        ))
    }

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
        let coordinator = make_coordinator(db.clone());
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
        let coordinator = make_coordinator(db.clone());
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

        let now_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            db.insert_terminal_execution_for_test(&work_item_id, "orphaned", now_epoch - i)
                .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone());
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(outcome.churn_skipped, 1, "churn guard should have fired");
        assert_eq!(outcome.refired, 0);
        assert!(sink.events().await.is_empty(), "no event on churn skip");
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
            .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET kind = 'pr_review', status = 'completed' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone());
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let checker = FakePrStateChecker::always(PrOpenState::Open);

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref(), &checker).await;

        assert_eq!(
            outcome.refired, 0,
            "a normally-completed review must not be treated as dead"
        );
        assert!(sink.events().await.is_empty());
    }
}
