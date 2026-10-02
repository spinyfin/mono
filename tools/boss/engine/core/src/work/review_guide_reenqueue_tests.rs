//! Startup re-enqueue of review guides that failed before start on an
//! earlier build ([`WorkDb::reenqueue_pre_start_failed_review_guides`]).

use super::*;
use crate::test_support::{create_active_chore, create_product, open_db, seed_review_guide_series_for_pr};

const PR_URL: &str = "https://github.com/acme/widget/pull/9";
const OLD_BUILD: &str = "build-old";
const NEW_BUILD: &str = "build-new";

/// An `in_review` task on `PR_URL` with a captured source series. Returns
/// `(root, series_id, comparison_id)`.
fn seeded_open_pr(db: &WorkDb) -> (String, String, String) {
    let root = create_active_chore(db, &create_product(db), "review guide re-enqueue test");
    db.update_work_item(
        &root,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(PR_URL.to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
            params!["https://github.com/acme/widget.git", root],
        )
        .unwrap();
    let (series_id, comparison_id) = seed_review_guide_series_for_pr(db, &root, PR_URL, "base", "head");
    (root, series_id, comparison_id)
}

fn attempt_for(db: &WorkDb, attempt_id: &str) -> PrReviewGuideAttempt {
    let conn = db.connect().unwrap();
    query_pr_review_guide_attempt(&conn, attempt_id).unwrap().unwrap()
}

/// A queued attempt for `comparison_id` that failed with `error` on `build`.
fn failed_attempt(
    db: &WorkDb,
    series_id: &str,
    comparison_id: &str,
    pre_start: bool,
    build: &str,
) -> PrReviewGuideAttempt {
    let attempt = db
        .create_pr_review_guide_attempt(series_id, comparison_id, "review-guide-v1")
        .unwrap();
    db.fail_pr_review_guide_attempt_at(&attempt.id, "spawn refused", pre_start, build)
        .unwrap();
    attempt_for(db, &attempt.id)
}

#[test]
fn pre_start_failure_is_reenqueued_after_a_build_change_but_not_on_the_same_build() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_open_pr(&db);
    let failed = failed_attempt(&db, &series_id, &comparison_id, true, OLD_BUILD);
    assert_eq!(failed.status, "failed");
    assert!(failed.failed_pre_start);
    assert_eq!(failed.failed_by_build.as_deref(), Some(OLD_BUILD));

    let same_build = db.reenqueue_pre_start_failed_review_guides(OLD_BUILD).unwrap();
    assert!(same_build.reenqueued.is_empty(), "{same_build:?}");

    let report = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
    assert_eq!(report.build, NEW_BUILD);
    assert_eq!(report.reenqueued.len(), 1, "{report:?}");
    let entry = &report.reenqueued[0];
    assert_eq!(entry.root_task_id, root);
    assert_eq!(entry.pr_url, PR_URL);
    assert_eq!(entry.failed_attempt_id, failed.id);

    // Admitted and dispatched like any other request: a bound attempt whose
    // execution is `ready`, so the coordinator applies the dispatch pause and
    // review pool when it claims it.
    let fresh = attempt_for(&db, &entry.new_attempt_id);
    assert_eq!(fresh.status, "running");
    assert_eq!(fresh.comparison_id, comparison_id);
    let execution = db.get_execution(fresh.execution_id.as_deref().unwrap()).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Ready);
    assert_eq!(execution.kind, ExecutionKind::PrReviewGuide);

    // History is untouched.
    let old = attempt_for(&db, &failed.id);
    assert_eq!(old.status, "failed");
    assert_eq!(old.error.as_deref(), Some("spawn refused"));

    // A second restart on the new build finds the live attempt and does nothing.
    let again = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
    assert!(again.reenqueued.is_empty(), "{again:?}");
}

#[test]
fn post_start_failures_are_not_reenqueued() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_open_pr(&db);
    let failed = failed_attempt(&db, &series_id, &comparison_id, false, OLD_BUILD);
    assert!(!failed.failed_pre_start);

    let report = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
    assert!(report.reenqueued.is_empty(), "{report:?}");
}

#[test]
fn attempts_that_failed_before_the_build_was_recorded_are_not_reenqueued() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_open_pr(&db);
    let failed = failed_attempt(&db, &series_id, &comparison_id, true, OLD_BUILD);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_guide_attempts SET failed_by_build = NULL WHERE id = ?1",
            [&failed.id],
        )
        .unwrap();

    let report = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
    assert!(report.reenqueued.is_empty(), "{report:?}");
}

#[test]
fn merged_closed_deleted_and_replaced_prs_are_not_reenqueued() {
    for (label, sql) in [
        (
            "merged or closed (done)",
            "UPDATE tasks SET status = 'done' WHERE id = ?1",
        ),
        (
            "closed (archived)",
            "UPDATE tasks SET status = 'archived' WHERE id = ?1",
        ),
        ("deleted", "UPDATE tasks SET deleted_at = datetime('now') WHERE id = ?1"),
        (
            "replaced by another PR",
            "UPDATE tasks SET pr_url = 'https://github.com/acme/widget/pull/10' WHERE id = ?1",
        ),
    ] {
        let (_dir, db) = open_db();
        let (root, series_id, comparison_id) = seeded_open_pr(&db);
        failed_attempt(&db, &series_id, &comparison_id, true, OLD_BUILD);
        db.connect().unwrap().execute(sql, [&root]).unwrap();

        let report = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
        assert!(report.reenqueued.is_empty(), "{label}: {report:?}");
    }
}

#[test]
fn series_with_a_newer_attempt_or_a_published_guide_are_not_reenqueued() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_open_pr(&db);
    failed_attempt(&db, &series_id, &comparison_id, true, OLD_BUILD);

    // A newer, still-live attempt (a human retried by hand).
    let newer = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    let report = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
    assert!(report.reenqueued.is_empty(), "newer live attempt: {report:?}");

    // The newer attempt publishes a guide for the same head.
    let PublishReviewGuideOutcome::Published(_) = db
        .publish_pr_review_guide_version(&newer.id, "# Guide\n", "raw")
        .unwrap()
    else {
        panic!("must publish")
    };
    let report = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
    assert!(report.reenqueued.is_empty(), "published guide: {report:?}");
}

#[test]
fn a_reenqueued_attempt_that_fails_again_is_not_retried_on_the_same_build() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_open_pr(&db);
    failed_attempt(&db, &series_id, &comparison_id, true, OLD_BUILD);

    let first = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
    assert_eq!(first.reenqueued.len(), 1);
    let retried = &first.reenqueued[0].new_attempt_id;

    // The retry fails before start again, on the build that retried it.
    db.fail_pr_review_guide_attempt_at(retried, "spawn refused again", true, NEW_BUILD)
        .unwrap();
    for _ in 0..2 {
        let report = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
        assert!(report.reenqueued.is_empty(), "no loop on one build: {report:?}");
    }

    // A later, different build gets exactly one more try.
    let next = db.reenqueue_pre_start_failed_review_guides("build-newer").unwrap();
    assert_eq!(next.reenqueued.len(), 1, "{next:?}");
    assert_eq!(next.reenqueued[0].failed_attempt_id, *retried);
}

#[test]
fn spawn_failures_record_whether_they_were_before_start_and_on_which_build() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_open_pr(&db);

    let pre_start = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    db.fail_pr_review_guide_attempt_pre_start(&pre_start.id, "refused")
        .unwrap();
    let recorded = attempt_for(&db, &pre_start.id);
    assert!(recorded.failed_pre_start);
    assert_eq!(
        recorded.failed_by_build.as_deref(),
        Some(crate::build_info::build_identity())
    );

    let post_start = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    db.fail_pr_review_guide_attempt(&post_start.id, "ran and gave up")
        .unwrap();
    assert!(!attempt_for(&db, &post_start.id).failed_pre_start);
}

#[test]
fn permanent_pre_start_execution_failure_marks_the_attempt_pre_start() {
    let (_dir, db) = open_db();
    let (root, _series_id, _comparison_id) = seeded_open_pr(&db);
    let RetryReviewGuideOutcome::Created(attempt) = db.retry_pr_review_guide(&root, None, "test").unwrap() else {
        panic!("must create attempt")
    };
    let execution_id = attempt.execution_id.expect("dispatched");

    let (_, _, outcome) = db
        .record_pre_start_failure(&execution_id, "agent", None, "hook-trust gate refused", &[])
        .unwrap();
    assert!(matches!(outcome, PreStartFailureOutcome::PermanentFail));

    let failed = attempt_for(&db, &attempt.id);
    assert_eq!(failed.status, "failed");
    assert!(failed.failed_pre_start);
    assert_eq!(failed.error.as_deref(), Some("hook-trust gate refused"));
}

#[test]
fn orphaned_execution_is_not_treated_as_a_pre_start_failure() {
    let (_dir, db) = open_db();
    let (root, _series_id, _comparison_id) = seeded_open_pr(&db);
    let RetryReviewGuideOutcome::Created(attempt) = db.retry_pr_review_guide(&root, None, "test").unwrap() else {
        panic!("must create attempt")
    };
    let execution_id = attempt.execution_id.expect("dispatched");
    db.finish_pr_review_guide_attempt_for_terminal_execution(
        &execution_id,
        ExecutionStatus::Orphaned,
        "execution orphaned",
        false,
    )
    .unwrap();

    let failed = attempt_for(&db, &attempt.id);
    assert_eq!(failed.status, "failed");
    assert!(!failed.failed_pre_start);
    let report = db.reenqueue_pre_start_failed_review_guides(NEW_BUILD).unwrap();
    assert!(report.reenqueued.is_empty(), "{report:?}");
}
