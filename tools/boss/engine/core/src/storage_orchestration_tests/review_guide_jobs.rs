//! Review-guide attempts across an engine restart, driven through the
//! validation layer above storage: a guide refused for a closed attempt is
//! published by startup recovery (or rolled back whole when the publish
//! fails), a re-adopted execution reopens its attempt, and hook usage is
//! captured durably per attempt, and a failed attempt records the engine build
//! that failed it.

use boss_protocol::{ExecutionStatus, WorkItemPatch};
use rusqlite::params;

use super::{next_id, now_string, raw};
use crate::test_support::{
    create_active_chore, create_product, open_db, seed_review_guide_series, seed_review_guide_series_for_pr,
};
use crate::work::{PublishReviewGuideOutcome, RetryReviewGuideOutcome, WorkDb};

const RESTART_GUIDE: &str = "# Guide\n## Orientation\nA change.\n## Problem\n## Implementation\n## Example\n## Review\n## Evidence\nNo validation results reported; no screenshots registered.";

fn seeded_series(db: &WorkDb) -> (String, String, String) {
    let product = create_product(db);
    let root = create_active_chore(db, &product, "review guide job test");
    raw(db)
        .execute(
            "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
            params!["https://github.com/acme/widget.git", root],
        )
        .unwrap();
    let (series_id, comparison_id) = seed_review_guide_series(db, &root);
    (root, series_id, comparison_id)
}

fn attempt_status(db: &WorkDb, attempt_id: &str) -> String {
    raw(db)
        .query_row(
            "SELECT status FROM pr_review_guide_attempts WHERE id = ?1",
            [attempt_id],
            |row| row.get(0),
        )
        .unwrap()
}

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

fn submit_guide(
    db: &WorkDb,
    execution_id: &str,
    comparison_id: &str,
) -> std::result::Result<crate::work::SubmitWorkerProposalOutcome, boss_protocol::ProposalSubmissionError> {
    let payload = serde_json::json!({ "body_markdown": RESTART_GUIDE }).to_string();
    crate::review_guide_validation::submit_worker_proposal(
        db,
        crate::work::SubmitWorkerProposalInput {
            execution_id,
            work_item_id: comparison_id,
            kind: boss_protocol::ProposalKind::ReviewGuide,
            payload_json: &payload,
            idempotency_key: "guide",
        },
    )
    .unwrap()
}

/// Run the startup recovery pass the way the engine does: validation above
/// storage, adoption and publication inside it.
fn recover_orphaned(db: &WorkDb) -> crate::review_guide_validation::ReviewGuideSubmissionRecoveryReport {
    crate::review_guide_validation::recover_orphaned_submissions(db).unwrap()
}

/// Insert the `rejected` row an older engine stored when a guide was
/// submitted against a closed attempt.
fn store_refused_guide_submission(db: &WorkDb, execution_id: &str, comparison_id: &str, body: &str) -> String {
    let id = next_id("prp");
    let payload = serde_json::json!({ "body_markdown": body }).to_string();
    raw(db)
        .execute(
            "INSERT INTO worker_proposals
                 (id, execution_id, work_item_id, kind, payload_json, idempotency_key,
                  state, applied_ref, decided_by, decision_reason, decided_at, created_at)
             VALUES (?1, ?2, ?3, 'review_guide', ?4, ?5, 'rejected', NULL, 'policy', ?6, ?7, ?7)",
            params![
                id,
                execution_id,
                comparison_id,
                payload,
                format!("key-{id}"),
                "this execution has no running review-guide attempt",
                now_string(),
            ],
        )
        .unwrap();
    id
}

fn proposal_state(db: &WorkDb, proposal_id: &str) -> (String, Option<String>) {
    raw(db)
        .query_row(
            "SELECT state, applied_ref FROM worker_proposals WHERE id = ?1",
            [proposal_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn review_guide_version_count(db: &WorkDb) -> i64 {
    raw(db)
        .query_row("SELECT COUNT(*) FROM pr_review_guide_versions", [], |row| row.get(0))
        .unwrap()
}

fn attempt_recovery_snapshot(
    db: &WorkDb,
    attempt_id: &str,
) -> (String, Option<String>, Option<String>, Option<String>) {
    raw(db)
        .query_row(
            "SELECT status, error, finished_at, failed_by_build FROM pr_review_guide_attempts WHERE id = ?1",
            [attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
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

/// The restart-during-spawn ordering. A guide run is spawned seconds before
/// the engine restarts; the restarted engine's recovery probe wrongly infers
/// the worker dead and orphans the execution, which closes the attempt with
/// it; the worker's next hook then re-adopts the execution. The attempt must
/// come back with the execution, and the guide the worker finishes minutes
/// later must bind to it and publish — instead of `propose review-guide`
/// failing with "no running review-guide attempt".
#[test]
fn restart_during_spawn_reopens_the_attempt_with_the_readopted_execution_and_publishes_the_guide() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let (attempt_id, execution_id) = spawned_guide_run(&db, &series_id, &comparison_id, "tok-28");

    // Restart. The startup reconcile sees a running execution: nothing to do.
    assert_eq!(
        db.reconcile_pr_review_guide_attempts_with_live_runs(&std::collections::HashSet::new(), &|_| false)
            .unwrap(),
        0
    );
    assert_eq!(attempt_status(&db, &attempt_id), "running");

    // The startup recovery probe wrongly proves the worker dead; the reap
    // orphans the execution and closes the attempt on the strength of it.
    db.reap_startup_dead_execution(&execution_id, &crate::run_reconcile::RunReconcileVerdict::Dead)
        .unwrap()
        .expect("the reap orphans the execution");
    assert_eq!(attempt_status(&db, &attempt_id), "failed");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
    // A guide submitted in this window is refused without storing anything.
    let refusal = submit_guide(&db, &execution_id, &comparison_id).expect_err("no running attempt");
    assert_eq!(refusal.code, boss_protocol::ProposalErrorCode::NotAccepted);

    // The worker's next hook disproves the death: re-adoption restores the
    // execution, and the attempt with it.
    let (restored, _) = db
        .readopt_inferred_terminal_execution(&execution_id, "hook_after_terminal", None)
        .unwrap();
    assert_eq!(restored.status, ExecutionStatus::Running);
    let reopened = db
        .pr_review_guide_attempt_for_execution(&execution_id)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.id, attempt_id);
    assert_eq!(reopened.status, "running");
    assert!(reopened.error.is_none() && reopened.finished_at.is_none());
    assert!(reopened.failed_by_build.is_none());
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "generating");

    // The guide the worker finished after the restart binds and publishes.
    let accepted = submit_guide(&db, &execution_id, &comparison_id).expect("the live attempt accepts the guide");
    assert!(!accepted.already_submitted);
    assert_eq!(accepted.proposal.state, boss_protocol::ProposalState::Applied);
    assert_eq!(accepted.proposal.applied_ref.as_deref(), Some(attempt_id.as_str()));
    let body = db
        .submitted_review_guide(&execution_id, &attempt_id)
        .unwrap()
        .expect("the applied proposal is the attempt's submission");
    let PublishReviewGuideOutcome::Published(version) =
        db.publish_pr_review_guide_version(&attempt_id, &body, &body).unwrap()
    else {
        panic!("must publish")
    };
    assert_eq!(version.markdown, RESTART_GUIDE);
    assert_eq!(attempt_status(&db, &attempt_id), "succeeded");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
}

/// A guide an older engine stored as a refused submission — its attempt
/// closed by an inferred death, the worker long gone — is published as the
/// PR's guide on startup: the attempt is reopened, the proposal is applied
/// against it, and the series' readable pointer advances. The pass is
/// idempotent.
#[test]
fn startup_recovery_publishes_a_guide_whose_submission_was_refused_for_a_closed_attempt() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let (attempt_id, execution_id) = spawned_guide_run(&db, &series_id, &comparison_id, "tok-32");
    db.mark_execution_orphaned(
        &execution_id,
        "engine startup: recovery probe proved worker dead across restart",
    )
    .unwrap();
    assert_eq!(attempt_status(&db, &attempt_id), "failed");
    let superseded_body = store_refused_guide_submission(
        &db,
        &execution_id,
        &comparison_id,
        "# Guide\n## Orientation\nOlder draft.\n## Problem\n## Implementation\n## Example\n## Review\n## Evidence\nNo validation results reported; no screenshots registered.",
    );
    let proposal_id = store_refused_guide_submission(&db, &execution_id, &comparison_id, RESTART_GUIDE);

    let report = recover_orphaned(&db);
    assert_eq!(report.skipped, Vec::<(String, String)>::new());
    assert_eq!(report.published.len(), 1);
    let (published_proposal, published_attempt, version_id) = &report.published[0];
    assert_eq!(
        (published_proposal.as_str(), published_attempt.as_str()),
        (proposal_id.as_str(), attempt_id.as_str())
    );
    assert_eq!(
        proposal_state(&db, &proposal_id),
        ("applied".to_owned(), Some(attempt_id.clone()))
    );
    assert_eq!(
        proposal_state(&db, &superseded_body).0,
        "rejected",
        "only the newest body is adopted"
    );
    assert_eq!(attempt_status(&db, &attempt_id), "succeeded");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
    assert_eq!(summary.readable_version_id.as_deref(), Some(version_id.as_str()));
    assert_eq!(
        db.get_pr_review_guide_version(version_id).unwrap().unwrap().markdown,
        RESTART_GUIDE
    );
    // The execution itself is left as recorded: recovery publishes the
    // guide, it does not resurrect the run.
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );

    assert!(recover_orphaned(&db).is_empty());
}

/// Recovery never publishes over a series that has moved on: once another
/// attempt is live for the series the stored body is left as history.
#[test]
fn startup_recovery_leaves_a_refused_guide_alone_when_the_series_has_a_live_attempt() {
    let (_dir, db) = open_db();
    let (_root, series_id, comparison_id) = seeded_series(&db);
    let (attempt_id, execution_id) = spawned_guide_run(&db, &series_id, &comparison_id, "tok-33");
    db.mark_execution_orphaned(
        &execution_id,
        "engine startup: recovery probe proved worker dead across restart",
    )
    .unwrap();
    let proposal_id = store_refused_guide_submission(&db, &execution_id, &comparison_id, RESTART_GUIDE);
    let replacement = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, boss_review_guide::PROMPT_VERSION)
        .unwrap();
    assert_ne!(replacement.id, attempt_id);

    assert!(recover_orphaned(&db).is_empty());
    assert_eq!(proposal_state(&db, &proposal_id).0, "rejected");
    assert_eq!(attempt_status(&db, &attempt_id), "failed");
}

/// Startup runs tmux re-adoption before recovery. When the worker's pane is
/// still live, re-adoption has already reopened the attempt to `running` by
/// the time recovery looks, and the stored legacy rejection must still be
/// published against that running attempt.
#[test]
fn startup_recovery_publishes_a_refused_guide_for_an_attempt_readoption_already_reopened() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let (attempt_id, execution_id) = spawned_guide_run(&db, &series_id, &comparison_id, "tok-34");
    db.mark_execution_orphaned(
        &execution_id,
        "engine startup: recovery probe proved worker dead across restart",
    )
    .unwrap();
    let proposal_id = store_refused_guide_submission(&db, &execution_id, &comparison_id, RESTART_GUIDE);
    db.readopt_inferred_terminal_execution(&execution_id, "hook_after_terminal", None)
        .unwrap();
    assert_eq!(attempt_status(&db, &attempt_id), "running");

    let report = recover_orphaned(&db);
    assert_eq!(report.skipped, Vec::<(String, String)>::new());
    assert_eq!(report.published.len(), 1);
    assert_eq!(
        proposal_state(&db, &proposal_id),
        ("applied".to_owned(), Some(attempt_id.clone()))
    );
    assert_eq!(attempt_status(&db, &attempt_id), "succeeded");
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
    assert!(recover_orphaned(&db).is_empty());
}

/// A refused insert of the version row after adopt must roll the whole
/// recovery transaction back: the proposal stays `rejected` with no
/// `applied_ref`, the attempt and series stay as the orphan reap left them,
/// and no version exists. Dropping the trigger and running again publishes
/// exactly once; a third pass is a no-op.
#[test]
fn startup_recovery_rolls_back_when_publish_insert_fails() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let (attempt_id, execution_id) = spawned_guide_run(&db, &series_id, &comparison_id, "tok-35");
    db.mark_execution_orphaned(
        &execution_id,
        "engine startup: recovery probe proved worker dead across restart",
    )
    .unwrap();
    let proposal_id = store_refused_guide_submission(&db, &execution_id, &comparison_id, RESTART_GUIDE);
    let attempt_before = attempt_recovery_snapshot(&db, &attempt_id);
    let summary_before = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(attempt_before.0, "failed");
    assert_eq!(summary_before.lifecycle, "failed");
    assert_eq!(summary_before.readable_version_id, None);

    raw(&db)
        .execute_batch(
            "CREATE TRIGGER reject_version BEFORE INSERT ON pr_review_guide_versions BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
    let report = recover_orphaned(&db);
    assert!(report.published.is_empty(), "{report:?}");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0, proposal_id);
    assert!(
        report.skipped[0].1.contains("injected"),
        "expected the injected abort in {:?}",
        report.skipped[0].1
    );
    assert_eq!(proposal_state(&db, &proposal_id), ("rejected".to_owned(), None));
    assert_eq!(attempt_recovery_snapshot(&db, &attempt_id), attempt_before);
    let summary_after_fail = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary_after_fail.lifecycle, summary_before.lifecycle);
    assert_eq!(summary_after_fail.readable_version_id, None);
    assert_eq!(review_guide_version_count(&db), 0);

    raw(&db).execute_batch("DROP TRIGGER reject_version").unwrap();
    let report = recover_orphaned(&db);
    assert_eq!(report.skipped, Vec::<(String, String)>::new());
    assert_eq!(report.published.len(), 1);
    assert_eq!(
        proposal_state(&db, &proposal_id),
        ("applied".to_owned(), Some(attempt_id.clone()))
    );
    assert_eq!(attempt_status(&db, &attempt_id), "succeeded");
    assert_eq!(review_guide_version_count(&db), 1);
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
    assert_eq!(
        summary.readable_version_id.as_deref(),
        Some(report.published[0].2.as_str())
    );

    assert!(recover_orphaned(&db).is_empty());
    assert_eq!(review_guide_version_count(&db), 1);
}

/// When publish fences with a non-`Published` outcome after adopt (here:
/// the series epoch moves mid-transaction so the insert never happens),
/// dropping the transaction rolls back the reopen and the proposal update.
/// A later unfenced pass still publishes exactly once.
#[test]
fn startup_recovery_rolls_back_when_publish_is_fenced_as_superseded() {
    let (_dir, db) = open_db();
    let (root, series_id, comparison_id) = seeded_series(&db);
    let (attempt_id, execution_id) = spawned_guide_run(&db, &series_id, &comparison_id, "tok-36");
    db.mark_execution_orphaned(
        &execution_id,
        "engine startup: recovery probe proved worker dead across restart",
    )
    .unwrap();
    let proposal_id = store_refused_guide_submission(&db, &execution_id, &comparison_id, RESTART_GUIDE);
    let attempt_before = attempt_recovery_snapshot(&db, &attempt_id);
    let epoch_before: i64 = raw(&db)
        .query_row(
            "SELECT request_epoch FROM pr_review_guide_source_series WHERE id = ?1",
            [&series_id],
            |row| row.get(0),
        )
        .unwrap();

    raw(&db)
        .execute_batch(
            "CREATE TRIGGER bump_epoch AFTER UPDATE OF status ON pr_review_guide_attempts
             WHEN NEW.status = 'running'
             BEGIN
               UPDATE pr_review_guide_source_series SET request_epoch = request_epoch + 1 WHERE id = NEW.series_id;
             END;",
        )
        .unwrap();
    let report = recover_orphaned(&db);
    assert!(report.published.is_empty(), "{report:?}");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0, proposal_id);
    assert!(
        report.skipped[0].1.contains("fenced"),
        "expected the publish-fence skip in {:?}",
        report.skipped[0].1
    );
    assert_eq!(proposal_state(&db, &proposal_id), ("rejected".to_owned(), None));
    assert_eq!(attempt_recovery_snapshot(&db, &attempt_id), attempt_before);
    let epoch_after: i64 = raw(&db)
        .query_row(
            "SELECT request_epoch FROM pr_review_guide_source_series WHERE id = ?1",
            [&series_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(epoch_after, epoch_before);
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "failed");
    assert_eq!(summary.readable_version_id, None);
    assert_eq!(review_guide_version_count(&db), 0);

    raw(&db).execute_batch("DROP TRIGGER bump_epoch").unwrap();
    let report = recover_orphaned(&db);
    assert_eq!(report.skipped, Vec::<(String, String)>::new());
    assert_eq!(report.published.len(), 1);
    assert_eq!(
        proposal_state(&db, &proposal_id),
        ("applied".to_owned(), Some(attempt_id.clone()))
    );
    assert_eq!(review_guide_version_count(&db), 1);
    assert!(recover_orphaned(&db).is_empty());
}

const PR_URL: &str = "https://github.com/acme/widget/pull/9";

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
    raw(db)
        .execute(
            "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
            params!["https://github.com/acme/widget.git", root],
        )
        .unwrap();
    let (series_id, comparison_id) = seed_review_guide_series_for_pr(db, &root, PR_URL, "base", "head");
    (root, series_id, comparison_id)
}

/// What the attempt row recorded about its failure: whether it was before
/// start, and on which build.
fn failure_columns(db: &WorkDb, attempt_id: &str) -> (bool, Option<String>) {
    raw(db)
        .query_row(
            "SELECT failed_pre_start, failed_by_build FROM pr_review_guide_attempts WHERE id = ?1",
            [attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
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
    let (failed_pre_start, failed_by_build) = failure_columns(&db, &pre_start.id);
    assert!(failed_pre_start);
    assert_eq!(failed_by_build.as_deref(), Some(crate::build_info::build_identity()));

    let post_start = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    db.fail_pr_review_guide_attempt(&post_start.id, "ran and gave up")
        .unwrap();
    assert!(!failure_columns(&db, &post_start.id).0);
}
