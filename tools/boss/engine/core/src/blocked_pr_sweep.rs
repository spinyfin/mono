//! Blocked-PR sweep: for every open Boss-tracked PR, say *why* it is not
//! progressing, re-kick the one engine path that has no other owner, and
//! store the answer where the Review column renders it.
//!
//! Runs at the end of every merge-poller pass (`merge_poller::run_one_pass_observed`),
//! i.e. on the poller's 60s cadence, after that pass has written the
//! lifecycle state (CI, mergeability, conflict / CI-fix attempts, reaped
//! review batches) the diagnosis reads. It makes no GitHub or Buildkite
//! calls of its own: the facts come from the work database
//! (`WorkDb::blocked_pr_facts`), the rules from
//! `boss_blocked_pr_diagnosis::diagnose`.
//!
//! Remediation here is deliberately limited to the gap no existing sweep
//! covers — a `todo` revision whose last execution ended terminal with no
//! successor (the orphan sweep handles `active` work items, the todo
//! promoter handles `waiting_dependency` ones). Transient CI retries happen
//! inside `ci_watch` where the failing checks are known
//! (`crate::ci_transient_retry`); conflict resolution, CI-fix revisions and
//! review re-admission already have owners, so for those the sweep only
//! renders their state. Every re-dispatch is a `pr_blocked_remediations`
//! row, and `boss_blocked_pr_diagnosis::DEAD_REVISION_REDISPATCH_BUDGET`
//! is counted from that table, per PR head.

use boss_blocked_pr_diagnosis::{Action, diagnose};
use boss_protocol::RequestExecutionInput;

use crate::coordinator::ExecutionPublisher;
use crate::work::{
    BlockedPrRemediationInput, PendingMergeCheck, REMEDIATION_CATEGORY_REVISION_REDISPATCH,
    REMEDIATION_OUTCOME_PENDING, REMEDIATION_OUTCOME_REQUEST_FAILED, REMEDIATION_OUTCOME_REQUESTED, WorkDb,
};

/// Per-pass counters, folded into the merge poller's `SweepOutcome`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BlockedPrSweepOutcome {
    /// Review-lane cards examined.
    pub examined: usize,
    /// Cards whose stored diagnosis was created or changed.
    pub diagnosed: usize,
    /// Cards whose stored diagnosis was removed because they are healthy.
    pub cleared: usize,
    /// Dead revisions re-dispatched.
    pub redispatched: usize,
    /// Cards whose diagnosis needs a human.
    pub needs_human: usize,
}

/// Diagnose every Review-lane card with a bound PR.
///
/// `review_fanout_enabled` is whether review-batch fanout is on, which
/// decides whether a failed review batch will be replaced.
pub async fn run_one_pass(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    review_fanout_enabled: bool,
) -> BlockedPrSweepOutcome {
    let mut outcome = BlockedPrSweepOutcome::default();
    let candidates = match work_db.list_blocked_pr_sweep_candidates() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(?err, "blocked-pr sweep: failed to list candidates");
            return outcome;
        }
    };
    for candidate in &candidates {
        diagnose_one(work_db, publisher, candidate, review_fanout_enabled, &mut outcome).await;
    }
    if outcome.diagnosed + outcome.cleared + outcome.redispatched > 0 {
        tracing::info!(
            examined = outcome.examined,
            diagnosed = outcome.diagnosed,
            cleared = outcome.cleared,
            redispatched = outcome.redispatched,
            needs_human = outcome.needs_human,
            "blocked-pr sweep: pass complete",
        );
    }
    outcome
}

/// Diagnose one card, apply its action, and store or clear its reason.
pub async fn diagnose_one(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    review_fanout_enabled: bool,
    outcome: &mut BlockedPrSweepOutcome,
) {
    outcome.examined += 1;
    let (facts, head_sha) = match work_db.blocked_pr_facts(&candidate.work_item_id, review_fanout_enabled) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(work_item_id = %candidate.work_item_id, ?err, "blocked-pr sweep: failed to gather facts");
            return;
        }
    };
    let Some(diagnosis) = diagnose(&facts) else {
        match work_db.clear_pr_blocked_diagnosis(&candidate.work_item_id) {
            Ok(true) => {
                outcome.cleared += 1;
                publisher
                    .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "blocked_pr_cleared")
                    .await;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(work_item_id = %candidate.work_item_id, ?err, "blocked-pr sweep: failed to clear diagnosis")
            }
        }
        return;
    };

    let action_label = match &diagnosis.action {
        Action::None => "none",
        Action::NeedsHuman => "needs_human",
        Action::RedispatchRevision { .. } => "redispatch_revision",
    };
    if let Action::RedispatchRevision { revision_id } = &diagnosis.action {
        let redispatched = redispatch_revision(work_db, publisher, candidate, head_sha.as_deref(), revision_id).await;
        if redispatched {
            outcome.redispatched += 1;
        }
    }
    if diagnosis.action == Action::NeedsHuman {
        outcome.needs_human += 1;
    }
    match work_db.upsert_pr_blocked_diagnosis(
        &candidate.work_item_id,
        diagnosis.category.key(),
        &diagnosis.reason,
        action_label,
    ) {
        Ok(true) => {
            outcome.diagnosed += 1;
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                category = diagnosis.category.key(),
                reason = %diagnosis.reason,
                action = action_label,
                "blocked-pr sweep: diagnosis",
            );
            publisher
                .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "blocked_pr_diagnosed")
                .await;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(work_item_id = %candidate.work_item_id, ?err, "blocked-pr sweep: failed to store diagnosis")
        }
    }
}

/// Re-request an execution for a revision whose last run died with no
/// successor — the same path `bossctl work start` uses. The diagnosis has
/// already checked the per-head budget. The ledger row is written as
/// `pending` before the request and finished afterwards, so the budget
/// holds across a crash in between, and a ledger that cannot be written
/// means no re-dispatch.
async fn redispatch_revision(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    head_sha: Option<&str>,
    revision_id: &str,
) -> bool {
    let reservation = BlockedPrRemediationInput::builder()
        .work_item_id(candidate.work_item_id.clone())
        .maybe_head_sha(head_sha)
        .category(REMEDIATION_CATEGORY_REVISION_REDISPATCH)
        .signal("dead_revision")
        .target(revision_id)
        .action("request_execution")
        .outcome(REMEDIATION_OUTCOME_PENDING)
        .build();
    let row_id = match work_db.record_pr_blocked_remediation(reservation) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                revision_id,
                ?err,
                "blocked-pr sweep: could not reserve a re-dispatch in the ledger; not re-dispatching",
            );
            return false;
        }
    };
    // Any still-live execution row means another path got here first; the
    // guard returns it and we must not stack a second one.
    let result = work_db.request_execution_with_live_check(
        RequestExecutionInput::builder()
            .work_item_id(revision_id.to_owned())
            .build(),
        |_| true,
    );
    let (outcome, detail) = match &result {
        Ok(exec) => (REMEDIATION_OUTCOME_REQUESTED, format!("execution {}", exec.id)),
        Err(err) => (REMEDIATION_OUTCOME_REQUEST_FAILED, format!("{err:#}")),
    };
    if let Err(err) = work_db.update_pr_blocked_remediation(row_id, outcome, &detail) {
        tracing::warn!(work_item_id = %candidate.work_item_id, ?err, "blocked-pr sweep: failed to finish re-dispatch ledger row");
    }
    match result {
        Ok(_) => {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                revision_id,
                detail = %detail,
                "blocked-pr sweep: re-dispatched dead revision",
            );
            publisher.kick_scheduler();
            true
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                revision_id,
                error = %format!("{err:#}"),
                "blocked-pr sweep: re-dispatch of dead revision refused",
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use boss_protocol::CreateRevisionInput;

    use super::*;
    use crate::test_support::*;
    use crate::work::{
        FakePrStateChecker, PrOpenState, REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY, REMEDIATION_OUTCOME_BUDGET_EXHAUSTED,
        REMEDIATION_OUTCOME_RETRIED, WorkItem, WorkItemPatch,
    };

    fn in_review_chore(db: &WorkDb, name: &str) -> PendingMergeCheck {
        let product = create_test_product_with_repo(db, &format!("Product-{name}"), Some("git@github.com:foo/bar.git"));
        let chore = create_test_chore_manual(db, product.id.clone(), name);
        let pr_url = format!("https://github.com/foo/bar/pull/{}", name.len());
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET ci_required_state = 'success', pr_mergeable_state = 'mergeable', pr_head_sha = 'head-1'
                 WHERE id = ?1",
                [&chore.id],
            )
            .unwrap();
        PendingMergeCheck {
            work_item_id: chore.id,
            product_id: product.id,
            pr_url,
        }
    }

    fn set_ci_failing(db: &WorkDb, chore: &str) {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET ci_required_state = 'fail',
                        ci_required_detail = '[{\"name\":\"buildkite/mono/checks\",\"conclusion\":\"FAILURE\"}]'
                 WHERE id = ?1",
                [chore],
            )
            .unwrap();
    }

    fn task(db: &WorkDb, id: &str) -> boss_protocol::Task {
        match db.get_work_item(id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => t,
            other => panic!("expected a chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn healthy_card_has_no_reason_and_a_failing_one_gets_one_then_loses_it() {
        let (_dir, db) = open_db();
        let publisher = Arc::new(RecordingPublisher::default());
        let c = in_review_chore(&db, "healthy");

        let outcome = run_one_pass(&db, publisher.as_ref(), true).await;
        assert_eq!(outcome.examined, 1);
        assert_eq!(outcome.diagnosed, 0);
        assert!(task(&db, &c.work_item_id).review_blocked_reason.is_none());

        set_ci_failing(&db, &c.work_item_id);
        let outcome = run_one_pass(&db, publisher.as_ref(), true).await;
        assert_eq!(outcome.diagnosed, 1);
        let t = task(&db, &c.work_item_id);
        assert_eq!(t.review_blocked_category.as_deref(), Some("ci_failing_no_remediation"));
        assert_eq!(
            t.review_blocked_reason.as_deref(),
            Some("CI failing on buildkite/mono/checks; no fix revision is running (attempts 0/3)")
        );
        assert!(
            publisher
                .lifecycle_reasons()
                .await
                .contains(&"blocked_pr_diagnosed".to_owned())
        );

        // Unchanged facts: no second publish.
        let outcome = run_one_pass(&db, publisher.as_ref(), true).await;
        assert_eq!(outcome.diagnosed, 0);

        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET ci_required_state = 'success' WHERE id = ?1",
                [&c.work_item_id],
            )
            .unwrap();
        let outcome = run_one_pass(&db, publisher.as_ref(), true).await;
        assert_eq!(outcome.cleared, 1);
        assert!(task(&db, &c.work_item_id).review_blocked_reason.is_none());
        assert!(
            publisher
                .lifecycle_reasons()
                .await
                .contains(&"blocked_pr_cleared".to_owned())
        );
    }

    #[tokio::test]
    async fn transient_retry_ledger_renders_retrying_then_exhausted() {
        let (_dir, db) = open_db();
        let publisher = Arc::new(RecordingPublisher::default());
        let c = in_review_chore(&db, "transient");
        set_ci_failing(&db, &c.work_item_id);
        let row = |outcome: &str, detail: &str| {
            BlockedPrRemediationInput::builder()
                .work_item_id(c.work_item_id.clone())
                .head_sha("head-1")
                .category(REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY)
                .signal("HTTP 504 in buildkite/mono/checks")
                .target("job-1")
                .action("bk job retry job-1")
                .outcome(outcome)
                .detail(detail)
                .build()
        };
        db.record_pr_blocked_remediation(row(REMEDIATION_OUTCOME_RETRIED, "1/2 upstream_unavailable: HTTP 504"))
            .unwrap();
        run_one_pass(&db, publisher.as_ref(), true).await;
        let t = task(&db, &c.work_item_id);
        assert_eq!(t.review_blocked_category.as_deref(), Some("ci_transient_retrying"));
        assert_eq!(
            t.review_blocked_reason.as_deref(),
            Some("CI retrying: HTTP 504 in buildkite/mono/checks (1/2)")
        );

        db.record_pr_blocked_remediation(row(REMEDIATION_OUTCOME_BUDGET_EXHAUSTED, "2/2 declined"))
            .unwrap();
        let outcome = run_one_pass(&db, publisher.as_ref(), true).await;
        assert_eq!(outcome.needs_human, 1);
        let t = task(&db, &c.work_item_id);
        assert_eq!(t.review_blocked_category.as_deref(), Some("ci_transient_exhausted"));
        assert_eq!(
            t.review_blocked_reason.as_deref(),
            Some(
                "Needs you: CI retry budget exhausted (2/2): HTTP 504 in buildkite/mono/checks; retry the job by hand"
            )
        );

        // A ledger row for another head does not leak onto this one.
        let c2 = in_review_chore(&db, "other-head");
        set_ci_failing(&db, &c2.work_item_id);
        run_one_pass(&db, publisher.as_ref(), true).await;
        assert_eq!(
            task(&db, &c2.work_item_id).review_blocked_category.as_deref(),
            Some("ci_failing_no_remediation")
        );
    }

    #[tokio::test]
    async fn dead_todo_revision_is_redispatched_once_and_the_card_says_so() {
        let (_dir, db) = open_db();
        let publisher = Arc::new(RecordingPublisher::default());
        let c = in_review_chore(&db, "dead-rev");
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(c.work_item_id.clone())
                    .description("Fix failing CI: checks")
                    .created_via("ci-fix:crm_test")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        // The worker died: its only execution ended `orphaned` long ago.
        let conn = db.connect().unwrap();
        let changed = conn
            .execute(
                "UPDATE work_executions SET status = 'orphaned', created_at = CAST(CAST(created_at AS INTEGER) - 3600 AS TEXT)
                 WHERE work_item_id = ?1",
                [&revision.id],
            )
            .unwrap();
        assert!(changed >= 1, "create_revision minted an initial execution");
        conn.execute("UPDATE tasks SET status = 'todo' WHERE id = ?1", [&revision.id])
            .unwrap();
        drop(conn);

        let outcome = run_one_pass(&db, publisher.as_ref(), true).await;
        assert_eq!(outcome.redispatched, 1, "{outcome:?}");
        let t = task(&db, &c.work_item_id);
        assert_eq!(t.review_blocked_category.as_deref(), Some("revision_dead"));
        assert!(
            t.review_blocked_reason
                .as_deref()
                .unwrap()
                .contains("has no running worker (last run orphaned); re-dispatching (1/2 on this PR head)"),
            "{:?}",
            t.review_blocked_reason
        );
        let live: i64 = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1
                 AND status NOT IN ('completed','failed','abandoned','cancelled','orphaned')",
                [&revision.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live, 1, "a fresh execution was requested");
        let ledger = db
            .list_pr_blocked_remediations(
                &c.work_item_id,
                crate::work::REMEDIATION_CATEGORY_REVISION_REDISPATCH,
                Some("head-1"),
                None,
            )
            .unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].outcome, REMEDIATION_OUTCOME_REQUESTED);

        // With a live execution the revision is no longer dead: the card
        // moves to In Revision and the reason is cleared.
        let outcome = run_one_pass(&db, publisher.as_ref(), true).await;
        assert_eq!(outcome.redispatched, 0);
        assert_eq!(outcome.cleared, 1);
        assert!(task(&db, &c.work_item_id).review_blocked_reason.is_none());
    }

    fn insert_batch(db: &WorkDb, id: &str, root: &str, target_sha: &str, status: &str, generation: i64) {
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO pr_review_batches
                     (id, cycle_root_id, base_sha, classification_json, created_at, phase, pr_number, pr_url,
                      status, target_sha, updated_at, generation, explicit)
                 VALUES (?1, ?2, 'base', '{}', ?3, 'pre_merge', 1, 'https://github.com/foo/bar/pull/1', ?4, ?5, ?3, ?6, 0)",
                rusqlite::params![id, root, format!("{generation}"), status, target_sha, generation],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn review_batch_diagnosis_is_scoped_to_the_current_head() {
        let (_dir, db) = open_db();
        let publisher = Arc::new(RecordingPublisher::default());
        let c = in_review_chore(&db, "batch-head");
        insert_batch(&db, "b-old", &c.work_item_id, "head-0", "failed", 2);
        let (facts, _) = db.blocked_pr_facts(&c.work_item_id, true).unwrap();
        assert!(
            facts.review.latest_pre_merge_batch.is_none(),
            "a failed batch for an earlier head is not the current head's batch"
        );
        run_one_pass(&db, publisher.as_ref(), true).await;
        assert!(task(&db, &c.work_item_id).review_blocked_reason.is_none());

        insert_batch(&db, "b-new", &c.work_item_id, "head-1", "failed", 1);
        let (facts, _) = db.blocked_pr_facts(&c.work_item_id, true).unwrap();
        let batch = facts.review.latest_pre_merge_batch.unwrap();
        assert_eq!((batch.status.as_str(), batch.generation), ("failed", 1));
        run_one_pass(&db, publisher.as_ref(), true).await;
        assert_eq!(
            task(&db, &c.work_item_id).review_blocked_category.as_deref(),
            Some("review_readmission_pending")
        );
        run_one_pass(&db, publisher.as_ref(), false).await;
        assert_eq!(
            task(&db, &c.work_item_id).review_blocked_category.as_deref(),
            Some("review_unreviewed_after_reap"),
            "with fanout off no replacement is coming"
        );
    }

    #[tokio::test]
    async fn redispatch_budget_counts_per_head_across_revisions_and_resets_on_a_new_head() {
        let (_dir, db) = open_db();
        let c = in_review_chore(&db, "head-budget");
        let ledger_row = |head: &str, revision: &str, outcome: &str| {
            db.record_pr_blocked_remediation(
                BlockedPrRemediationInput::builder()
                    .work_item_id(c.work_item_id.clone())
                    .head_sha(head)
                    .category(REMEDIATION_CATEGORY_REVISION_REDISPATCH)
                    .signal("dead_revision")
                    .target(revision)
                    .action("request_execution")
                    .outcome(outcome)
                    .build(),
            )
            .unwrap()
        };
        // Two different revisions, one head: shared allowance. A pending
        // reservation counts like a finished request.
        ledger_row("head-1", "rev_a", REMEDIATION_OUTCOME_REQUESTED);
        ledger_row("head-1", "rev_b", REMEDIATION_OUTCOME_PENDING);
        let (facts, _) = db.blocked_pr_facts(&c.work_item_id, true).unwrap();
        assert_eq!(facts.head_redispatches_used, 2);
        // A different head starts over.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_head_sha = 'head-2' WHERE id = ?1",
                [&c.work_item_id],
            )
            .unwrap();
        let (facts, _) = db.blocked_pr_facts(&c.work_item_id, true).unwrap();
        assert_eq!(facts.head_redispatches_used, 0);
    }

    #[tokio::test]
    async fn review_verdicts_are_read_for_the_current_head_only() {
        let (_dir, db) = open_db();
        let c = in_review_chore(&db, "verdict-head");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET review_required_state = 'required' WHERE id = ?1",
                [&c.work_item_id],
            )
            .unwrap();
        let (facts, _) = db.blocked_pr_facts(&c.work_item_id, true).unwrap();
        assert_eq!(facts.review.review_required_state.as_deref(), Some("required"));
        assert!(facts.review.head_known && facts.review.head_verdict.is_none());
    }

    #[tokio::test]
    async fn blocked_rule_matches_classify_review_section() {
        // Parity between the pure crate's Blocked rule and the engine's
        // section classifier, over the inputs both read.
        use boss_blocked_pr_diagnosis::{BlockedFacts, RevisionFacts, is_review_blocked};
        use boss_protocol::{ReviewSection, TaskKind, TaskStatus};
        // (status, blocked_reason, ci_required_state, pr_mergeable_state, open revision status)
        type Case = (
            &'static str,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
        );
        let cases: [Case; 6] = [
            ("in_review", None, Some("success"), Some("mergeable"), None),
            ("in_review", None, Some("fail"), Some("mergeable"), None),
            ("in_review", None, Some("success"), Some("conflicting"), None),
            ("blocked", Some("ci_failure"), Some("fail"), Some("mergeable"), None),
            ("in_review", None, Some("in_progress"), Some("unknown"), Some("blocked")),
            ("in_review", None, Some("in_progress"), Some("unknown"), Some("active")),
        ];
        for (status, reason, ci, mergeable, revision_status) in cases {
            let mut t = boss_protocol::Task::builder()
                .id("t1")
                .product_id("p")
                .kind(TaskKind::Chore)
                .name("n")
                .description("")
                .status(if status == "blocked" {
                    TaskStatus::Blocked
                } else {
                    TaskStatus::InReview
                })
                .created_at("")
                .updated_at("")
                .pr_url("https://github.com/o/r/pull/1")
                .build();
            t.blocked_reason = reason.map(str::to_owned);
            t.ci_required_state = ci.map(str::to_owned);
            t.pr_mergeable_state = mergeable.map(str::to_owned);
            if let Some(rs) = revision_status {
                t.has_in_progress_revision = true;
                t.in_revision_blocked_only = rs == "blocked";
            }
            let engine_blocked = crate::work::classify_review_section(&t) == Some(ReviewSection::Blocked);
            let facts = BlockedFacts {
                status: status.into(),
                blocked_reason: reason.map(str::to_owned),
                ci_required_state: ci.map(str::to_owned),
                pr_mergeable_state: mergeable.map(str::to_owned),
                revisions: revision_status
                    .map(|rs| {
                        vec![RevisionFacts {
                            id: "r".into(),
                            status: rs.into(),
                            ..Default::default()
                        }]
                    })
                    .unwrap_or_default(),
                ..Default::default()
            };
            assert_eq!(
                is_review_blocked(&facts),
                engine_blocked,
                "{status} {reason:?} {ci:?} {mergeable:?} {revision_status:?}"
            );
        }
    }
}
