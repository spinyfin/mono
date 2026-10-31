use std::collections::HashSet;

use super::*;
use crate::test_support::{
    create_active_chore, create_product, open_db, review_guide_source_packet, seed_review_guide_series,
};

fn seeded_series(db: &WorkDb) -> (String, String, String) {
    let product = create_product(db);
    let root = create_active_chore(db, &product, "review guide job test");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
            params!["https://github.com/acme/widget.git", root],
        )
        .unwrap();
    let (series_id, comparison_id) = seed_review_guide_series(db, &root);
    (root, series_id, comparison_id)
}

#[test]
fn attempt_then_execution_then_publish_advances_the_readable_pointer() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let attempt = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    assert_eq!(attempt.status, "queued");
    assert_eq!(attempt.request_epoch, 1);

    let execution = db
        .create_pr_review_guide_execution(&comparison_id, "acme/widget")
        .unwrap();
    assert_eq!(execution.work_item_id, comparison_id);
    assert_eq!(execution.kind, boss_protocol::ExecutionKind::PrReviewGuide);
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();

    let bound = db
        .pr_review_guide_attempt_for_execution(&execution.id)
        .unwrap()
        .unwrap();
    assert_eq!(bound.status, "running");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "generating");

    let outcome = db
        .publish_pr_review_guide_version(&attempt.id, "# Guide\n\n## Problem\n", "raw")
        .unwrap();
    let PublishReviewGuideOutcome::Published(version) = outcome else {
        panic!("must publish")
    };
    assert_eq!(version.markdown, "# Guide\n\n## Problem\n");

    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
    assert_eq!(summary.readable_version_id.as_deref(), Some(version.id.as_str()));

    let fetched = db.get_pr_review_guide_version(&version.id).unwrap().unwrap();
    assert_eq!(fetched.markdown, version.markdown);
}

/// A review-guide run's `work_executions.work_item_id` is the comparison
/// id, not the task it reviewed — the task-keyed transcripts/executions
/// surface (`WorkDb::list_executions`) must still resolve the owning task
/// via the comparison -> series -> root-task join and include the run,
/// labelled by its own `kind`, rather than leaving it reachable only by
/// its opaque comparison id.
#[test]
fn list_executions_for_task_includes_its_review_guide_runs() {
    let (_dir, db) = open_db();
    let (root, _series_id, comparison_id) = seeded_series(&db);
    let execution = db
        .create_pr_review_guide_execution(&comparison_id, "acme/widget")
        .unwrap();

    let executions = db.list_executions(Some(&root)).unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].id, execution.id);
    assert_eq!(executions[0].work_item_id, comparison_id);
    assert_eq!(executions[0].kind, boss_protocol::ExecutionKind::PrReviewGuide);
    assert_eq!(executions[0].owning_task_id.as_deref(), Some(root.as_str()));

    let chain_executions = db.list_executions_for_chain(&root).unwrap();
    assert_eq!(chain_executions.len(), 1);
    assert_eq!(chain_executions[0].id, execution.id);
    assert_eq!(chain_executions[0].owning_task_id.as_deref(), Some(root.as_str()));

    // An unrelated task's history must not pick up the guide run.
    let other_root = create_active_chore(&db, &create_product(&db), "unrelated task");
    assert!(db.list_executions(Some(&other_root)).unwrap().is_empty());
}

/// A `PrReviewGuide` execution's `work_item_id` is a comparison id
/// (`prgc_…`), not a task/project/product/comment id. Every reconciliation
/// path that classifies a bound `work_item_id` must recognise that shape
/// instead of failing with "unknown work item id format" — the failure
/// mode this test guards against.
#[test]
fn comparison_ids_are_a_recognised_work_item_id_kind() {
    let (_dir, db) = open_db();
    let (_root, _series_id, comparison_id) = seeded_series(&db);

    // A comparison never has an independent open/closed lifecycle of its
    // own (like a product or project), so this must resolve rather than
    // bail with "unknown work item id format".
    assert!(!db.is_bound_work_item_closed(&comparison_id).unwrap());
    assert!(!db.work_item_row_missing(&comparison_id).unwrap());

    // A genuinely unknown comparison id (never captured) is a confirmed
    // miss, not a classification failure.
    assert!(db.work_item_row_missing("prgc_never_captured").unwrap());
}

#[test]
fn publish_for_a_superseded_comparison_does_not_advance_the_pointer() {
    let (_dir, db) = open_db();
    let (root, series_id, first_comparison) = seeded_series(&db);
    let first_attempt = db
        .create_pr_review_guide_attempt(&series_id, &first_comparison, "review-guide-v1")
        .unwrap();

    // A newer comparison is captured and selected while the first attempt
    // is still in flight.
    db.persist_pr_review_guide_source_capture(
        &root,
        2,
        PrSourceCaptureTrigger::Poller,
        &review_guide_source_packet("base2", "head2"),
    )
    .unwrap();

    let outcome = db
        .publish_pr_review_guide_version(&first_attempt.id, "# Stale guide\n\n## X\n## Y\n## Z\n## W\n", "raw")
        .unwrap();
    assert_eq!(outcome, PublishReviewGuideOutcome::Superseded);

    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(
        summary.lifecycle, "queued",
        "stale publish must not flip lifecycle to ready"
    );
    assert!(summary.readable_version_id.is_none());
}

#[test]
fn publish_after_terminal_is_a_noop() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_series(&db);
    let attempt = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    db.fail_pr_review_guide_attempt(&attempt.id, "boom").unwrap();
    let outcome = db
        .publish_pr_review_guide_version(&attempt.id, "# Late\n\n## A\n## B\n## C\n## D\n", "raw")
        .unwrap();
    assert_eq!(outcome, PublishReviewGuideOutcome::AlreadyTerminal);
}

#[test]
fn failure_records_error_without_touching_a_prior_readable_version() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let first = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    db.publish_pr_review_guide_version(&first.id, "# Good\n\n## A\n## B\n## C\n## D\n", "raw")
        .unwrap();
    let readable_before = db
        .get_pr_review_guide_summary_for_root(&root)
        .unwrap()
        .unwrap()
        .readable_version_id;

    // Explicit regenerate against the SAME comparison; this attempt fails.
    let retry = db.retry_pr_review_guide(&root, None, "review-guide-v1").unwrap();
    let RetryReviewGuideOutcome::Created(retry_attempt) = retry else {
        panic!("retry must create a new attempt")
    };
    db.fail_pr_review_guide_attempt(&retry_attempt.id, "model unavailable")
        .unwrap();

    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
    assert_eq!(
        summary.readable_version_id, readable_before,
        "old readable version must survive a failed refresh"
    );
}

#[test]
fn retry_with_the_same_idempotency_token_returns_the_original_attempt() {
    let (_dir, db) = open_db();
    let (root, ..) = seeded_series(&db);
    let first = db
        .retry_pr_review_guide(&root, Some("tok-1"), "review-guide-v1")
        .unwrap();
    let RetryReviewGuideOutcome::Created(first_attempt) = first else {
        panic!("first call must create")
    };
    let second = db
        .retry_pr_review_guide(&root, Some("tok-1"), "review-guide-v1")
        .unwrap();
    let RetryReviewGuideOutcome::AlreadyRequested(second_attempt) = second else {
        panic!("repeat token must not create a second attempt")
    };
    assert_eq!(first_attempt.id, second_attempt.id);
    assert!(
        first_attempt.execution_id.is_some(),
        "retry must create and bind a work_executions row, not leave the attempt queued forever"
    );
    assert_eq!(first_attempt.execution_id, second_attempt.execution_id);
    assert_eq!(first_attempt.status, "running");
}

#[test]
fn retry_without_any_captured_comparison_reports_no_comparison() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "no capture yet");
    assert_eq!(
        db.retry_pr_review_guide(&root, None, "review-guide-v1").unwrap(),
        RetryReviewGuideOutcome::NoComparison
    );
}

/// A failure records exactly the build the caller names, or none at all when
/// the build is unknowable. A failure with no build is never a candidate for
/// the build-change retry, which only considers attempts with a recorded build.
#[test]
fn a_failure_records_the_named_build_or_none_when_it_is_unknowable() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_series(&db);
    let failed_by_build = |attempt_id: &str| -> Option<String> {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT failed_by_build FROM pr_review_guide_attempts WHERE id = ?1",
                [attempt_id],
                |row| row.get(0),
            )
            .unwrap()
    };

    let named = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    db.fail_pr_review_guide_attempt_at(&named.id, "spawn refused", true, "build-a")
        .unwrap();
    assert_eq!(failed_by_build(&named.id).as_deref(), Some("build-a"));

    let unknown = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    db.fail_pr_review_guide_attempt_for_build(&unknown.id, "spawn refused", true, None)
        .unwrap();
    assert_eq!(failed_by_build(&unknown.id), None);
    assert_eq!(attempt_status(&db, &unknown.id), "failed");
}

/// Failure provenance belongs to the database instance, including clones.
#[test]
fn failure_operations_record_the_database_build_and_remain_retry_eligible() {
    for build in ["engine-one", "engine-two"] {
        let (_dir, db) = open_db();
        let db = db.with_engine_build_identity(build);
        let (root, series_id, comparison_id) = seeded_series(&db);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET pr_url = 'https://github.com/acme/widget/pull/9' WHERE id = ?1",
                [&root],
            )
            .unwrap();
        let attempt = db
            .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
            .unwrap();
        let execution = db
            .create_pr_review_guide_execution(&comparison_id, "acme/widget")
            .unwrap();
        db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();
        db.clone()
            .finish_pr_review_guide_attempt_for_terminal_execution(
                &execution.id,
                ExecutionStatus::Failed,
                "pane never came up",
                true,
            )
            .unwrap();
        let recorded = query_pr_review_guide_attempt(&db.connect().unwrap(), &attempt.id)
            .unwrap()
            .unwrap();
        assert!(recorded.failed_pre_start);
        assert_eq!(recorded.failed_by_build.as_deref(), Some(build));
        assert_eq!(
            db.reenqueue_pre_start_failed_review_guides("next-build", &FakePrStateChecker::always(PrOpenState::Open))
                .unwrap()
                .reenqueued
                .len(),
            1
        );
    }
}

fn attempt_status(db: &WorkDb, attempt_id: &str) -> String {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT status FROM pr_review_guide_attempts WHERE id = ?1",
            [attempt_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn attempt_error(db: &WorkDb, attempt_id: &str) -> String {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT error FROM pr_review_guide_attempts WHERE id = ?1",
            [attempt_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .unwrap_or_default()
}

#[test]
fn failing_a_stale_attempt_does_not_downgrade_a_newer_ready_series() {
    let (_dir, db) = open_db();
    let (root, series_id, first_comparison) = seeded_series(&db);
    let first = db
        .create_pr_review_guide_attempt(&series_id, &first_comparison, "review-guide-v1")
        .unwrap();

    db.persist_pr_review_guide_source_capture(
        &root,
        2,
        PrSourceCaptureTrigger::Poller,
        &review_guide_source_packet("base2", "head2"),
    )
    .unwrap();
    let second_comparison = db
        .get_pr_review_guide_summary_for_root(&root)
        .unwrap()
        .unwrap()
        .selected_comparison_id
        .unwrap();
    let second = db
        .create_pr_review_guide_attempt(&series_id, &second_comparison, "review-guide-v1")
        .unwrap();
    db.publish_pr_review_guide_version(&second.id, "# Current\n\n## A\n## B\n## C\n## D\n", "raw")
        .unwrap();

    db.fail_pr_review_guide_attempt(&first.id, "late invalid output")
        .unwrap();

    assert_eq!(attempt_status(&db, &first.id), "superseded");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
}

#[test]
fn reconcile_finishes_a_running_attempt_whose_execution_is_already_terminal() {
    let (_dir, db) = open_db();
    let db = db.with_engine_build_identity("reconcile-build");
    let (root, series_id, comparison_id) = seeded_series(&db);
    let attempt = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    let execution = db
        .create_pr_review_guide_execution(&comparison_id, "https://github.com/acme/widget.git")
        .unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    // Terminalize the execution row without going through cancel_execution,
    // which is the stranded shape the sweep has to recover: orphan reap,
    // abandon, or a restart that lost the run.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned', finished_at = datetime('now') WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();

    let acted = db
        .reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
        .unwrap();
    assert_eq!(acted, 1);
    assert_eq!(attempt_status(&db, &attempt.id), "failed");
    let recorded = db
        .pr_review_guide_attempt_for_execution(&execution.id)
        .unwrap()
        .unwrap();
    assert_eq!(recorded.failed_by_build.as_deref(), Some("reconcile-build"));
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
}

#[test]
fn reconcile_prefers_execution_last_error_over_generic_status_reason() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let attempt = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    let execution = db
        .create_pr_review_guide_execution(&comparison_id, "https://github.com/acme/widget.git")
        .unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions
             SET status = 'failed', finished_at = datetime('now'),
                 last_error = 'Codex hook-trust gate refused the session'
             WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();

    let acted = db
        .reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
        .unwrap();
    assert_eq!(acted, 1);
    assert_eq!(attempt_status(&db, &attempt.id), "failed");
    let stored: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT error FROM pr_review_guide_attempts WHERE id = ?1",
            [&attempt.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, "Codex hook-trust gate refused the session");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(
        summary.error.as_deref(),
        Some("Codex hook-trust gate refused the session"),
    );
}

#[test]
fn record_pre_start_failure_fails_bound_review_guide_attempt_with_cause_chain() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let attempt = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    let execution = db
        .create_pr_review_guide_execution(&comparison_id, "https://github.com/acme/widget.git")
        .unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();

    let chained = anyhow::anyhow!("inner cause").context("outer label");
    let (execution, _run, outcome) = db
        .record_pre_start_failure(&execution.id, "worker-1", None, &format!("{chained:#}"), &[])
        .unwrap();
    assert!(matches!(outcome, PreStartFailureOutcome::PermanentFail));
    assert!(
        execution
            .last_error
            .as_deref()
            .is_some_and(|err| err.contains("inner cause") && err.contains("outer label")),
        "permanent pre-start failure must persist the full cause chain; got {:?}",
        execution.last_error,
    );
    assert_eq!(attempt_status(&db, &attempt.id), "failed");
    assert!(
        attempt_error(&db, &attempt.id).contains("inner cause"),
        "bound attempt error must carry the inner cause; got {:?}",
        attempt_error(&db, &attempt.id),
    );
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
    assert!(
        summary.error.as_deref().is_some_and(|err| err.contains("inner cause")),
        "summary error must carry the inner cause; got {:?}",
        summary.error,
    );
}

#[test]
fn summary_projects_latest_cancelled_attempt_error_not_an_older_failure() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let first = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    db.fail_pr_review_guide_attempt(&first.id, "attempt A failed").unwrap();
    let second = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    db.cancel_pr_review_guide_attempt(&second.id, "attempt B cancelled")
        .unwrap();

    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
    assert_eq!(summary.error.as_deref(), Some("attempt B cancelled"));
}

#[test]
fn admission_enforces_one_active_attempt_per_series() {
    let (_dir, db) = open_db();
    let (root, series_id, _first_comparison) = seeded_series(&db);
    let first = db.retry_pr_review_guide(&root, None, "review-guide-v1").unwrap();
    let RetryReviewGuideOutcome::Created(first) = first else {
        panic!("must create")
    };
    assert_eq!(first.status, "running");

    db.persist_pr_review_guide_source_capture(
        &root,
        2,
        PrSourceCaptureTrigger::Poller,
        &review_guide_source_packet("base2", "head2"),
    )
    .unwrap();
    let RetryReviewGuideOutcome::Created(replacement) =
        db.retry_pr_review_guide(&root, None, "review-guide-v1").unwrap()
    else {
        panic!("must create replacement")
    };
    let live = db.live_pr_review_guide_attempts_for_series(&series_id).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, replacement.id);

    let status: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT status FROM pr_review_guide_attempts WHERE id = ?1",
            [&first.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "superseded",
        "the first comparison is no longer selected, so admission records superseded not failed"
    );
    let exec = db.get_execution(first.execution_id.as_deref().unwrap()).unwrap();
    assert_eq!(exec.status, ExecutionStatus::Cancelled);
}

#[test]
fn cancel_execution_writes_cancelled_on_the_bound_attempt() {
    let (_dir, db) = open_db();
    let (root, ..) = seeded_series(&db);
    let RetryReviewGuideOutcome::Created(attempt) = db.retry_pr_review_guide(&root, None, "review-guide-v1").unwrap()
    else {
        panic!("must create")
    };
    db.cancel_execution(attempt.execution_id.as_deref().unwrap()).unwrap();
    assert_eq!(attempt_status(&db, &attempt.id), "cancelled");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
}

#[test]
fn reconcile_dispatches_a_queued_attempt_with_no_execution() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let attempt = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    assert!(attempt.execution_id.is_none());

    let acted = db
        .reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
        .unwrap();
    assert_eq!(acted, 1);
    let bound = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT execution_id, status FROM pr_review_guide_attempts WHERE id = ?1",
            [&attempt.id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap();
    assert!(bound.0.is_some());
    assert_eq!(bound.1, "running");
    let _ = root;
}

#[test]
fn repeated_token_recovers_an_unbound_attempt() {
    let (_dir, db) = open_db();
    let (root, series, comparison) = seeded_series(&db);
    let attempt = db
        .create_pr_review_guide_attempt_with_token(&series, &comparison, "test", Some("recover"))
        .unwrap();
    let RetryReviewGuideOutcome::AlreadyRequested(bound) =
        db.retry_pr_review_guide(&root, Some("recover"), "test").unwrap()
    else {
        panic!("must recover")
    };
    assert_eq!(bound.id, attempt.id);
    assert!(bound.execution_id.is_some());
}

#[test]
fn reconcile_adopts_execution_inserted_before_a_crash() {
    let (_dir, db) = open_db();
    let (_root, series, comparison) = seeded_series(&db);
    let attempt = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
    let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
    assert_eq!(
        db.reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
            .unwrap(),
        1
    );
    let bound = db
        .pr_review_guide_attempt_for_execution(&execution.id)
        .unwrap()
        .unwrap();
    assert_eq!(bound.id, attempt.id);
    let count: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1",
            [&comparison],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn concurrent_dispatch_creates_only_one_execution() {
    let (_dir, db) = open_db();
    let (root, series, comparison) = seeded_series(&db);
    let attempt = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let dispatch = || {
            barrier.wait();
            db.dispatch_pr_review_guide_attempt(&attempt.id, &root).unwrap()
        };
        let first = scope.spawn(dispatch);
        let second = scope.spawn(dispatch);
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(results.0.execution_id, results.1.execution_id);
    let count: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1",
            [&comparison],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn concurrent_enqueue_and_retry_leave_one_live_attempt() {
    let (_dir, db) = open_db();
    let (root, series, comparison) = seeded_series(&db);
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let enqueue = scope.spawn(|| {
            barrier.wait();
            let a = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            db.dispatch_pr_review_guide_attempt(&a.id, &root).unwrap();
        });
        let retry = scope.spawn(|| {
            barrier.wait();
            db.retry_pr_review_guide(&root, None, "test").unwrap();
        });
        enqueue.join().unwrap();
        retry.join().unwrap();
    });
    assert_eq!(db.live_pr_review_guide_attempts_for_series(&series).unwrap().len(), 1);
    let count: i64 = db.connect().unwrap().query_row(
        "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1 AND status NOT IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')",
        [&comparison], |row| row.get(0)).unwrap();
    assert_eq!(count, 1);
}

#[test]
fn stale_epoch_cannot_publish_or_fail_the_same_comparison() {
    let (_dir, db) = open_db();
    let (root, series, comparison) = seeded_series(&db);
    for publish in [true, false] {
        let attempt = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
        db.connect().unwrap().execute(
            "UPDATE pr_review_guide_source_series SET request_epoch = request_epoch + 1, guide_lifecycle = 'ready' WHERE id = ?1",
            [&series]).unwrap();
        if publish {
            assert_eq!(
                db.publish_pr_review_guide_version(&attempt.id, "# Late", "raw")
                    .unwrap(),
                PublishReviewGuideOutcome::Superseded
            );
        } else {
            db.fail_pr_review_guide_attempt(&attempt.id, "late failure").unwrap();
        }
        assert_eq!(attempt_status(&db, &attempt.id), "superseded");
        assert_eq!(
            db.get_pr_review_guide_summary_for_root(&root)
                .unwrap()
                .unwrap()
                .lifecycle,
            "ready"
        );
    }
}

#[test]
fn replacement_failure_rolls_back_cancellation_and_lifecycle() {
    let (_dir, db) = open_db();
    let (root, ..) = seeded_series(&db);
    let RetryReviewGuideOutcome::Created(first) = db.retry_pr_review_guide(&root, None, "test").unwrap() else {
        panic!("must create")
    };
    db.connect().unwrap().execute_batch(
        "CREATE TRIGGER reject_attempt BEFORE INSERT ON pr_review_guide_attempts BEGIN SELECT RAISE(ABORT, 'injected admission failure'); END;"
    ).unwrap();
    assert!(db.retry_pr_review_guide(&root, None, "test").is_err());
    assert_eq!(attempt_status(&db, &first.id), "running");
    assert_eq!(
        db.get_execution(first.execution_id.as_deref().unwrap()).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(
        db.get_pr_review_guide_summary_for_root(&root)
            .unwrap()
            .unwrap()
            .lifecycle,
        "generating"
    );
}

#[test]
fn cancellation_failure_prevents_replacement() {
    let (_dir, db) = open_db();
    let (root, series, _) = seeded_series(&db);
    let RetryReviewGuideOutcome::Created(first) = db.retry_pr_review_guide(&root, None, "test").unwrap() else {
        panic!("must create")
    };
    db.connect().unwrap().execute_batch(
        "CREATE TRIGGER reject_cancel BEFORE UPDATE OF status ON work_executions WHEN NEW.status = 'cancelled' BEGIN SELECT RAISE(ABORT, 'injected cancellation failure'); END;"
    ).unwrap();
    assert!(db.retry_pr_review_guide(&root, None, "test").is_err());
    let live = db.live_pr_review_guide_attempts_for_series(&series).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, first.id);
}

#[test]
fn bind_failure_rolls_back_insert_and_terminal_bind_preserves_lifecycle() {
    let (_dir, db) = open_db();
    let (root, series, comparison) = seeded_series(&db);
    let attempt = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
    db.connect().unwrap().execute_batch(
        "CREATE TRIGGER reject_bind BEFORE UPDATE OF execution_id ON pr_review_guide_attempts BEGIN SELECT RAISE(ABORT, 'injected bind failure'); END;"
    ).unwrap();
    assert!(db.dispatch_pr_review_guide_attempt(&attempt.id, &root).is_err());
    let count: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1",
            [&comparison],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    db.connect().unwrap().execute_batch("DROP TRIGGER reject_bind").unwrap();
    db.cancel_pr_review_guide_attempt(&attempt.id, "cancel").unwrap();
    let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
    assert!(
        db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
            .is_err()
    );
    assert_eq!(
        db.get_pr_review_guide_summary_for_root(&root)
            .unwrap()
            .unwrap()
            .lifecycle,
        "failed"
    );
}

#[test]
fn reconcile_bounds_dispatch_failures_and_fails_missing_repository() {
    for missing_repo in [true, false] {
        let (_dir, db) = open_db();
        let (root, series, comparison) = seeded_series(&db);
        let attempt = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
        if missing_repo {
            db.connect()
                .unwrap()
                .execute("UPDATE tasks SET repo_remote_url = NULL WHERE id = ?1", [&root])
                .unwrap();
            db.connect().unwrap().execute(
                "UPDATE products SET repo_remote_url = NULL WHERE id = (SELECT product_id FROM tasks WHERE id = ?1)",
                [&root],
            ).unwrap();
            let error = db.dispatch_pr_review_guide_attempt(&attempt.id, &root).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("has no repository; cannot dispatch review-guide attempt")
            );
        } else {
            db.connect().unwrap().execute_batch(
                "CREATE TRIGGER reject_execution BEFORE INSERT ON work_executions BEGIN SELECT RAISE(ABORT, 'injected dispatch failure'); END;"
            ).unwrap();
            assert_eq!(
                db.reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
                    .unwrap(),
                0
            );
            assert_eq!(
                db.reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
                    .unwrap(),
                0
            );
        }
        assert_eq!(
            db.reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
                .unwrap(),
            1
        );
        assert_eq!(attempt_status(&db, &attempt.id), "failed");
        assert_eq!(
            db.reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
                .unwrap(),
            0
        );
        assert_eq!(
            db.get_pr_review_guide_summary_for_root(&root)
                .unwrap()
                .unwrap()
                .lifecycle,
            "failed"
        );
    }
}

#[test]
fn retry_dispatch_resolves_product_repository_and_task_override() {
    let product_repo = "https://github.com/acme/widget.git";
    let override_repo = "https://github.com/acme/override.git";
    for task_repo in [None, Some(override_repo)] {
        let (_dir, db) = open_db();
        let product = crate::test_support::create_test_product_with_repo(&db, "retry repository", Some(product_repo));
        let root = create_active_chore(&db, &product.id, "retry repository resolution");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
                params![task_repo, root],
            )
            .unwrap();
        seed_review_guide_series(&db, &root);

        let RetryReviewGuideOutcome::Created(attempt) = db.retry_pr_review_guide(&root, None, "test").unwrap() else {
            panic!("must create retry")
        };
        let execution = db
            .get_execution(attempt.execution_id.as_deref().expect("retry must dispatch"))
            .unwrap();
        assert_eq!(execution.repo_remote_url, task_repo.unwrap_or(product_repo));
        assert_eq!(execution.kind, ExecutionKind::PrReviewGuide);
        assert_eq!(execution.status, ExecutionStatus::Ready);
    }
}

// ---------------------------------------------------------------------------
// Attempts across an engine restart
// ---------------------------------------------------------------------------

/// A bound attempt whose execution has started a run with a live tmux shell
/// (`shell_pid` is this test process). Returns `(attempt_id, execution_id)`.
fn spawned_guide_run(db: &WorkDb, series_id: &str, comparison_id: &str, token: &str) -> (String, String) {
    let attempt = db
        .create_pr_review_guide_attempt(series_id, comparison_id, boss_review_guide::PROMPT_VERSION)
        .unwrap();
    let execution = db
        .create_pr_review_guide_execution(comparison_id, "https://github.com/acme/widget.git")
        .unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    db.start_execution_run(&execution.id, "review-28", "mono", "lease-28", "ws-28", "/tmp/ws-28")
        .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution.id, "boss", "boss-review-28", token)
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution.id, token, i64::from(std::process::id()))
            .unwrap()
    );
    (attempt.id, execution.id)
}

/// A pid that is guaranteed dead: spawn `true` and reap it.
fn released_pid() -> i64 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    child.wait().unwrap();
    i64::from(child.id())
}

/// The reconcile judges an `orphaned` execution by whether its worker is
/// live, not by the status alone: an attempt that is still `running` against
/// an orphaned row with a live shell (or one the caller observed alive) is
/// kept open for re-adoption, and is finished only once nothing is alive.
#[test]
fn reconcile_keeps_the_attempt_of_an_orphaned_execution_whose_worker_is_live() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let (attempt_id, execution_id) = spawned_guide_run(&db, &series_id, &comparison_id, "tok-29");
    // Orphan the row without closing the attempt — the shape left behind by
    // a resume that orphans its predecessor, or a reap interrupted between
    // its two writes.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned', finished_at = datetime('now') WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();

    // An injected live observation keeps the attempt open without an OS probe.
    assert_eq!(
        db.reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|id| {
            assert_eq!(id, execution_id);
            true
        })
        .unwrap(),
        0
    );
    assert_eq!(attempt_status(&db, &attempt_id), "running");

    // No live pid, but the caller (the tmux adoption pass) observed the
    // worker alive: still kept open.
    assert!(
        db.set_run_shell_pid_for_execution(&execution_id, released_pid())
            .unwrap()
    );
    let known_live = HashSet::from([execution_id.clone()]);
    assert_eq!(
        db.reconcile_pr_review_guide_attempts_with_live_runs(&known_live, &|_| panic!(
            "caller evidence must take precedence"
        ))
        .unwrap(),
        0
    );
    assert_eq!(attempt_status(&db, &attempt_id), "running");

    // Nothing alive anywhere: the attempt really is stranded and is finished
    // with the classified reason.
    assert_eq!(
        db.reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
            .unwrap(),
        1
    );
    assert_eq!(attempt_status(&db, &attempt_id), "failed");
    assert_eq!(attempt_error(&db, &attempt_id), "execution orphaned");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
}

/// A deliberate terminal status is never a candidate for keeping the
/// attempt open, however alive the process behind it is.
#[test]
fn reconcile_finishes_the_attempt_of_a_deliberately_terminal_execution_even_with_a_live_pid() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_series(&db);
    let (attempt_id, execution_id) = spawned_guide_run(&db, &series_id, &comparison_id, "tok-30");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'abandoned', finished_at = datetime('now') WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    let known_live = HashSet::from([execution_id.clone()]);
    assert_eq!(
        db.reconcile_pr_review_guide_attempts_with_live_runs(&known_live, &|_| panic!(
            "caller evidence must take precedence"
        ))
        .unwrap(),
        1
    );
    assert_eq!(attempt_status(&db, &attempt_id), "failed");
    assert_eq!(attempt_error(&db, &attempt_id), "execution abandoned");
}

/// Re-adoption reverses only the closure the inferred death caused: an
/// attempt that failed before any worker ran stays failed (it is the
/// re-enqueue pass's business), and a published attempt is never reopened.
#[test]
fn readoption_does_not_reopen_a_pre_start_failure() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_series(&db);
    let (attempt_id, execution_id) = spawned_guide_run(&db, &series_id, &comparison_id, "tok-31");
    db.fail_pr_review_guide_attempt_pre_start(&attempt_id, "pane never came up")
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned', finished_at = datetime('now') WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    db.readopt_inferred_terminal_execution(&execution_id, "hook_after_terminal", None)
        .unwrap();
    assert_eq!(attempt_status(&db, &attempt_id), "failed");
    assert_eq!(attempt_error(&db, &attempt_id), "pane never came up");
}
