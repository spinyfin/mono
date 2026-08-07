//! `pr_review_verdicts`: `latest_review_verdict`'s "no verdict yet" and
//! most-recent-wins contracts. The other read (`review_verdict_for_execution`)
//! and the full `gate_outcome` derivation are covered end-to-end in
//! `completion::tests::t04` via `finalize_pr_review_pass`.

use super::*;

/// Stand up a `pr_review` execution attached to a fresh chore so a verdict
/// row has a real `execution_id` to satisfy the FK to `work_executions`.
fn seed_pr_review_execution(db: &WorkDb, product_id: &str, label: &str) -> (Task, WorkExecution) {
    let chore = create_test_chore_manual(db, product_id, format!("review target {label}"));
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    (chore, execution)
}

fn insert_verdict(db: &WorkDb, execution_id: &str, work_item_id: &str, created_at: &str) {
    insert_verdict_with_outcome(
        db,
        execution_id,
        work_item_id,
        created_at,
        crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
    );
}

fn insert_verdict_with_outcome(
    db: &WorkDb,
    execution_id: &str,
    work_item_id: &str,
    created_at: &str,
    gate_outcome: &'static str,
) {
    let conn = db.connect().unwrap();
    WorkDb::insert_review_verdict_in_tx(
        &conn,
        execution_id,
        work_item_id,
        &crate::work::ReviewVerdictInput {
            head_sha: Some("sha1".to_owned()),
            findings_count: 0,
            revision_warranted: false,
            gate_outcome,
        },
    )
    .unwrap();
    // `insert_review_verdict_in_tx` always stamps `created_at` with
    // `now_string()`; overwrite it so ordering is deterministic instead of
    // racing the clock's second-granularity within a single test.
    conn.execute(
        "UPDATE pr_review_verdicts SET created_at = ?2 WHERE execution_id = ?1",
        rusqlite::params![execution_id, created_at],
    )
    .unwrap();
}

/// `None` for a work item that has never had a `pr_review` pass complete —
/// callers must treat this as "unknown", never infer a clean verdict from it.
#[test]
fn latest_review_verdict_is_none_for_work_item_with_no_verdict() {
    let db = WorkDb::open(temp_db_path("latest-verdict-none")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "never reviewed");

    assert!(db.latest_review_verdict(&chore.id).unwrap().is_none());
}

/// Across a multi-pass revision chain, the most recently created verdict
/// row for a work item wins, not the first one inserted.
#[test]
fn latest_review_verdict_returns_most_recent_by_created_at() {
    let db = WorkDb::open(temp_db_path("latest-verdict-ordering")).unwrap();
    let product = create_test_product(&db);
    let (chore, exec_a) = seed_pr_review_execution(&db, &product.id, "a");
    let (_chore2, exec_b) = seed_pr_review_execution(&db, &product.id, "b");

    // Both verdicts recorded against the SAME work item id, as a multi-pass
    // revision chain would (the chain's later pass's verdict is what a
    // caller wants when asking "what's the latest state of this work item").
    insert_verdict(&db, &exec_a.id, &chore.id, "100");
    insert_verdict(&db, &exec_b.id, &chore.id, "200");

    let latest = db.latest_review_verdict(&chore.id).unwrap().expect("a verdict exists");
    assert_eq!(
        latest.execution_id, exec_b.id,
        "the later-created verdict must win, not insertion order"
    );

    // Reversed insertion order must not change the outcome.
    let db2 = WorkDb::open(temp_db_path("latest-verdict-ordering-reversed")).unwrap();
    let product2 = create_test_product(&db2);
    let (chore2, exec_c) = seed_pr_review_execution(&db2, &product2.id, "c");
    let (_chore3, exec_d) = seed_pr_review_execution(&db2, &product2.id, "d");
    insert_verdict(&db2, &exec_d.id, &chore2.id, "200");
    insert_verdict(&db2, &exec_c.id, &chore2.id, "100");
    let latest2 = db2
        .latest_review_verdict(&chore2.id)
        .unwrap()
        .expect("a verdict exists");
    assert_eq!(latest2.execution_id, exec_d.id);
}

/// `review_verdicts_for_work_item` returns every pass for a work item,
/// most-recent-first — the full attempt history `bossctl review show`
/// renders, not just the latest row.
#[test]
fn review_verdicts_for_work_item_returns_full_history_most_recent_first() {
    let db = WorkDb::open(temp_db_path("verdict-history")).unwrap();
    let product = create_test_product(&db);
    let (chore, exec_a) = seed_pr_review_execution(&db, &product.id, "a");
    let (_chore2, exec_b) = seed_pr_review_execution(&db, &product.id, "b");
    let (_chore3, exec_c) = seed_pr_review_execution(&db, &product.id, "c");

    insert_verdict(&db, &exec_a.id, &chore.id, "100");
    insert_verdict(&db, &exec_b.id, &chore.id, "200");
    insert_verdict(&db, &exec_c.id, &chore.id, "300");

    let history = db.review_verdicts_for_work_item(&chore.id).unwrap();
    let execution_ids: Vec<&str> = history.iter().map(|v| v.execution_id.as_str()).collect();
    assert_eq!(
        execution_ids,
        vec![exec_c.id.as_str(), exec_b.id.as_str(), exec_a.id.as_str()],
        "history must come back newest-first"
    );
}

/// `review_verdicts_for_work_item` for a work item with no `pr_review` pass
/// at all comes back empty, not an error.
#[test]
fn review_verdicts_for_work_item_is_empty_for_never_reviewed_item() {
    let db = WorkDb::open(temp_db_path("verdict-history-empty")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "never reviewed");

    assert!(db.review_verdicts_for_work_item(&chore.id).unwrap().is_empty());
}

/// `is_informative_gate_outcome` distinguishes the two outcomes that
/// represent a real completed judgement (`completed_clean`,
/// `completed_with_findings`) from the two that don't (`gave_up`,
/// `dropped_duplicate_head`) — the same distinction the AI-review badge
/// resolver applies via `query_latest_informative_review_verdicts`, and
/// that `bossctl review show` applies inline via
/// `history.iter().find(|v| is_informative_gate_outcome(&v.gate_outcome))`
/// to find the latest informative verdict in a full history returned by
/// `review_verdicts_for_work_item`.
#[test]
fn is_informative_gate_outcome_distinguishes_completed_from_abandoned() {
    assert!(crate::work::is_informative_gate_outcome(
        crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN
    ));
    assert!(crate::work::is_informative_gate_outcome(
        crate::work::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS
    ));
    assert!(crate::work::is_informative_gate_outcome(
        crate::work::REVIEW_GATE_OUTCOME_REVISION_CREATION_FAILED
    ));
    assert!(!crate::work::is_informative_gate_outcome(
        crate::work::REVIEW_GATE_OUTCOME_GAVE_UP
    ));
    assert!(!crate::work::is_informative_gate_outcome(
        crate::work::REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD
    ));
}

/// `review_verdicts_for_work_item` combined with an
/// `is_informative_gate_outcome` filter — the pattern `bossctl review show`
/// uses — skips past a `gave_up` / `dropped_duplicate_head` pass (neither is
/// positive evidence of anything) and finds the most recent pass that
/// actually represents a completed judgement, even when it isn't the
/// newest row.
#[test]
fn history_filtered_by_informative_outcome_skips_abandoned_passes() {
    let db = WorkDb::open(temp_db_path("verdict-informative")).unwrap();
    let product = create_test_product(&db);
    let (chore, exec_a) = seed_pr_review_execution(&db, &product.id, "a");
    let (_chore2, exec_b) = seed_pr_review_execution(&db, &product.id, "b");
    let (_chore3, exec_c) = seed_pr_review_execution(&db, &product.id, "c");

    // Oldest pass: a real clean verdict.
    insert_verdict_with_outcome(
        &db,
        &exec_a.id,
        &chore.id,
        "100",
        crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
    );
    // Most recent two passes: neither is informative.
    insert_verdict_with_outcome(
        &db,
        &exec_b.id,
        &chore.id,
        "200",
        crate::work::REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD,
    );
    insert_verdict_with_outcome(
        &db,
        &exec_c.id,
        &chore.id,
        "300",
        crate::work::REVIEW_GATE_OUTCOME_GAVE_UP,
    );

    let history = db.review_verdicts_for_work_item(&chore.id).unwrap();
    let latest_informative = history
        .iter()
        .find(|v| crate::work::is_informative_gate_outcome(&v.gate_outcome))
        .expect("the earlier clean verdict must still be found");
    assert_eq!(
        latest_informative.execution_id, exec_a.id,
        "must skip past the two non-informative passes and return the earlier clean one"
    );

    // No informative pass at all -> None, distinct from an empty history.
    let db2 = WorkDb::open(temp_db_path("verdict-informative-none")).unwrap();
    let product2 = create_test_product(&db2);
    let (chore2, exec_d) = seed_pr_review_execution(&db2, &product2.id, "d");
    insert_verdict_with_outcome(
        &db2,
        &exec_d.id,
        &chore2.id,
        "100",
        crate::work::REVIEW_GATE_OUTCOME_GAVE_UP,
    );
    let history2 = db2.review_verdicts_for_work_item(&chore2.id).unwrap();
    assert!(
        !history2
            .iter()
            .any(|v| crate::work::is_informative_gate_outcome(&v.gate_outcome))
    );
}
