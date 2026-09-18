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

    let acted = db.reconcile_pr_review_guide_attempts().unwrap();
    assert_eq!(acted, 1);
    assert_eq!(attempt_status(&db, &attempt.id), "failed");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
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

    let acted = db.reconcile_pr_review_guide_attempts().unwrap();
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
    assert_eq!(db.reconcile_pr_review_guide_attempts().unwrap(), 1);
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
        } else {
            db.connect().unwrap().execute_batch(
                "CREATE TRIGGER reject_execution BEFORE INSERT ON work_executions BEGIN SELECT RAISE(ABORT, 'injected dispatch failure'); END;"
            ).unwrap();
            assert_eq!(db.reconcile_pr_review_guide_attempts().unwrap(), 0);
            assert_eq!(db.reconcile_pr_review_guide_attempts().unwrap(), 0);
        }
        assert_eq!(db.reconcile_pr_review_guide_attempts().unwrap(), 1);
        assert_eq!(attempt_status(&db, &attempt.id), "failed");
        assert_eq!(db.reconcile_pr_review_guide_attempts().unwrap(), 0);
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
fn migration_retires_legacy_duplicates_and_enforces_one_live_series() {
    let (_dir, db) = open_db();
    let (root, series, comparison) = seeded_series(&db);
    let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
    let first = db.dispatch_pr_review_guide_attempt(&first.id, &root).unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch("DROP INDEX pr_review_guide_attempts_one_live_series")
        .unwrap();
    conn.execute(
        "INSERT INTO pr_review_guide_attempts (id, series_id, comparison_id, request_epoch, ordinal, status, prompt_version, created_at)
         VALUES ('legacy-newer', ?1, ?2, 2, 2, 'queued', 'test', datetime('now'))",
        params![series, comparison]).unwrap();
    migrate_pr_review_guide_job_tables(&conn).unwrap();
    migrate_pr_review_guide_job_tables(&conn).unwrap();
    assert_eq!(
        query_pr_review_guide_attempt(&conn, &first.id).unwrap().unwrap().status,
        "superseded"
    );
    assert_eq!(
        query_execution(&conn, first.execution_id.as_deref().unwrap())
            .unwrap()
            .unwrap()
            .status,
        ExecutionStatus::Cancelled
    );
    assert!(conn.execute(
        "INSERT INTO pr_review_guide_attempts (id, series_id, comparison_id, request_epoch, ordinal, status, prompt_version, created_at)
         VALUES ('duplicate', ?1, ?2, 3, 3, 'queued', 'test', datetime('now'))",
        params![series, comparison]).is_err());
}

#[tokio::test]
async fn hook_usage_is_lossless_durable_and_isolated_between_attempts() {
    let (dir, db) = open_db();
    let (root, _, _) = seeded_series(&db);
    let RetryReviewGuideOutcome::Created(first) = db.retry_pr_review_guide(&root, None, "test").unwrap() else {
        panic!("must create");
    };
    let path = dir.path().join("usage.jsonl");
    let usage = serde_json::json!({
        "input_tokens": 100, "cached_input_tokens": 80, "output_tokens": 20,
        "reasoning_output_tokens": 12, "future_category": {"tokens": 3}
    });
    let record = serde_json::json!({"type": "event_msg", "payload": {
        "type": "token_count", "info": {"total_token_usage": usage}
    }});
    std::fs::write(&path, format!("{record}\n")).unwrap();
    let capture = crate::run_cost::RunCostCapture::new();
    let execution_id = first.execution_id.as_deref().unwrap();
    capture
        .capture_and_persist(&db, execution_id, &path, None)
        .await
        .unwrap();
    // Repeated hooks replace cumulative observations; they do not add totals.
    capture
        .capture_and_persist(&db, execution_id, &path, None)
        .await
        .unwrap();
    let read_usage = || {
        let attempt = db.pr_review_guide_attempt_for_execution(execution_id).unwrap().unwrap();
        serde_json::from_str::<serde_json::Value>(&attempt.provider_usage_json.unwrap()).unwrap()
    };
    let stored = read_usage();
    assert_eq!(stored.as_object().unwrap().len(), 1);
    assert_eq!(stored[format!("codex:{}", path.display())]["total_token_usage"], usage);
    assert!(
        stored[format!("codex:{}", path.display())]["total_token_usage"]
            .get("cache_write_input_tokens")
            .is_none()
    );
    let RetryReviewGuideOutcome::Created(second) = db.retry_pr_review_guide(&root, None, "test").unwrap() else {
        panic!("must create retry");
    };
    assert!(second.provider_usage_json.is_none());
    assert_eq!(read_usage(), stored, "superseded attempts retain usage");
    // Truncating the transcript clears obsolete usage, including on a terminal attempt.
    std::fs::write(&path, "").unwrap();
    capture
        .capture_and_persist(&db, execution_id, &path, None)
        .await
        .unwrap();
    assert!(
        db.pr_review_guide_attempt_for_execution(execution_id)
            .unwrap()
            .unwrap()
            .provider_usage_json
            .is_none()
    );
}
