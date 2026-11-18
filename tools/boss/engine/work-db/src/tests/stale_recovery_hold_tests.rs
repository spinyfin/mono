use super::*;

// ── A recovery hold does not outlive the launch that disproves it ─────────
//
// Contract: `blocked: execution_recovery_failed` records that one attempt
// could not establish recovery before it started, with a detail describing
// the shared store as that attempt found it. Recovery is decided again from
// the store before every start, so the hold says nothing about a later
// attempt. A run that starts lifts it, and so does the explicit start that
// asks for that run. The worker of the new run then owns the row: a question
// it asks parks the row on that question and stays open.
//
// The shape this pins: a review revision asked the owner a question; another
// revision of the same PR rebased the branch meanwhile, so relaunching the
// first one was refused before start and the row was blocked for recovery;
// the store was repaired and the row restarted; every later run asked its
// question on a row still blocked for recovery, so the question was filed as
// withdrawn (`task_not_parkable`) and the card kept the old failure text.

/// The detail an earlier engine wrote when preserved work looked uncontained
/// in a PR head rebased by another revision. It names bookmark targets that
/// were true only at that attempt.
const STALE_DETAIL: &str = "preserved work of execution exec_prior is not contained in PR head pr/42 (28f2ec23): \
     boss-recovery/exec_prior at 7f082787 has 1 change(s) with no variant in it";

fn blocked_fields(db: &WorkDb, task_id: &str) -> (Option<String>, Option<String>) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT blocked_reason, blocked_detail FROM tasks WHERE id = ?1",
            [task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn start_run(db: &WorkDb, execution_id: &str, worker: &str) {
    db.start_execution_run(
        execution_id,
        worker,
        "mono",
        &format!("lease-{worker}"),
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
}

fn split_question() -> boss_protocol::OperatorQuestion {
    boss_protocol::OperatorQuestion {
        text: "Split this change into two pull requests?".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
        explanation: "The change touches more files than one pull request may.".into(),
    }
}

/// Block `task_id` the way a refused pre-start recovery did, and fail the
/// attempt that was refused.
fn refuse_for_recovery(db: &WorkDb, task_id: &str, execution_id: &str) {
    db.update_work_item_with_basis(
        task_id,
        boss_protocol::WorkItemPatch::builder()
            .status("blocked")
            .blocked_reason("execution_recovery_failed")
            .blocked_detail(STALE_DETAIL)
            .autostart(false)
            .build(),
        "engine",
        "execution_bookmark_recovery:block_unrecoverable",
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'failed', finished_at = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, now_string()],
        )
        .unwrap();
    assert_eq!(
        blocked_fields(db, task_id),
        (Some("execution_recovery_failed".into()), Some(STALE_DETAIL.into()))
    );
}

/// A review revision and a merge-conflict revision of the same PR; the first
/// one's relaunch has just been refused for recovery.
fn revision_blocked_for_recovery(db: &WorkDb, label: &str) -> String {
    let product = make_revision_product(db, label);
    let root = make_in_review_chore(db, &product, "https://github.com/spinyfin/mono/pull/42");
    let revision = insert_revision_row(db, &product, &root);
    db.reconcile_product_executions(&product).unwrap();
    let refused = executions_for(db, &revision)[0].0.clone();
    refuse_for_recovery(db, &revision, &refused);
    revision
}

fn ask(db: &WorkDb, execution_id: &str) {
    let question = split_question();
    crate::pr_flow::insert_question(&db.connect().unwrap(), execution_id, "prop_split", &question).unwrap();
    db.record_worker_awaiting_operator_answer(execution_id, "cannot finish without a decision", &question)
        .unwrap()
        .expect("a live execution records its question");
}

#[test]
fn a_question_from_a_run_relaunched_after_a_recovery_failure_stays_open() {
    let db = WorkDb::open(temp_db_path("recovery-hold-question")).unwrap();
    let revision = revision_blocked_for_recovery(&db, "recovery-hold-question");

    // The store is repaired and the row is restarted explicitly. The hold
    // described the refused attempt, so it goes with the restart.
    let relaunched = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision)
                .explicit_start(true)
                .build(),
        )
        .unwrap();
    assert_eq!(relaunched.status, ExecutionStatus::Ready);
    assert_eq!(task_status(&db, &revision), "todo");
    assert_eq!(blocked_fields(&db, &revision), (None, None));

    start_run(&db, &relaunched.id, "worker-1");
    assert_eq!(task_status(&db, &revision), "active");

    ask(&db, &relaunched.id);

    assert_eq!(task_status(&db, &revision), "blocked");
    assert_eq!(
        blocked_fields(&db, &revision),
        (Some("awaiting_operator_answer".into()), Some(split_question().text),),
        "the row waits on the question, not on the earlier recovery failure"
    );
    let questions = db.list_operator_questions(&revision).unwrap();
    assert_eq!(questions.len(), 1, "{questions:?}");
    assert_eq!(questions[0].status, boss_protocol::OperatorQuestionStatus::Open);
    assert_eq!(questions[0].withdrawn_reason, None);

    // It stays open across reconcile ticks until it is answered.
    let product = db
        .connect()
        .unwrap()
        .query_row("SELECT product_id FROM tasks WHERE id = ?1", [&revision], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    for _ in 0..3 {
        db.reconcile_product_executions(&product).unwrap();
    }
    assert_eq!(
        db.list_operator_questions(&revision).unwrap()[0].status,
        boss_protocol::OperatorQuestionStatus::Open
    );
    assert_eq!(
        blocked_fields(&db, &revision).0.as_deref(),
        Some("awaiting_operator_answer")
    );
}

/// A run can start without an explicit start (a redispatch minted by an
/// answered question, a recovery sweep). The start itself lifts the hold, so
/// the rule does not depend on which caller asked for the run.
#[test]
fn a_starting_run_lifts_the_recovery_hold_and_its_stale_detail() {
    let db = WorkDb::open(temp_db_path("recovery-hold-run-start")).unwrap();
    let revision = revision_blocked_for_recovery(&db, "recovery-hold-run-start");
    let minted = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert_eq!(
        blocked_fields(&db, &revision),
        (Some("execution_recovery_failed".into()), Some(STALE_DETAIL.into())),
        "minting an execution alone proves nothing about recovery"
    );

    start_run(&db, &minted.id, "worker-1");

    assert_eq!(task_status(&db, &revision), "active");
    assert_eq!(blocked_fields(&db, &revision), (None, None));
    let audited: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT basis FROM task_status_audit
             WHERE work_item_id = ?1 AND property = 'status' AND new_value = 'active'
             ORDER BY id DESC LIMIT 1",
            [&revision],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audited, "start_execution_run_clears_recovery_hold");

    ask(&db, &minted.id);
    let questions = db.list_operator_questions(&revision).unwrap();
    assert_eq!(questions[0].status, boss_protocol::OperatorQuestionStatus::Open);
    assert_eq!(
        blocked_fields(&db, &revision),
        (Some("awaiting_operator_answer".into()), Some(split_question().text),)
    );
}

/// The lift is narrow. A review starting on the row says nothing about
/// implementation recovery, and a hold another part of the engine owns is
/// not a recovery hold: a question asked under it is still not parked.
#[test]
fn only_an_implementation_start_lifts_only_the_recovery_hold() {
    let db = WorkDb::open(temp_db_path("recovery-hold-narrow")).unwrap();
    let revision = revision_blocked_for_recovery(&db, "recovery-hold-narrow");
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    start_run(&db, &review.id, "reviewer-1");
    assert_eq!(task_status(&db, &revision), "blocked");
    assert_eq!(
        blocked_fields(&db, &revision),
        (Some("execution_recovery_failed".into()), Some(STALE_DETAIL.into())),
        "a review start does not lift an implementation recovery hold"
    );

    let other = revision_blocked_for_recovery(&db, "recovery-hold-other");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET blocked_reason = 'merge_conflict', blocked_detail = 'conflicts with main' WHERE id = ?1",
            [&other],
        )
        .unwrap();
    let run = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(other.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    start_run(&db, &run.id, "worker-2");
    assert_eq!(
        blocked_fields(&db, &other),
        (Some("merge_conflict".into()), Some("conflicts with main".into())),
        "a hold the conflict watch owns is left to it"
    );
    ask(&db, &run.id);
    let questions = db.list_operator_questions(&other).unwrap();
    assert_eq!(questions[0].status, boss_protocol::OperatorQuestionStatus::Withdrawn);
    assert_eq!(questions[0].withdrawn_reason.as_deref(), Some("task_not_parkable"));
    assert_eq!(blocked_fields(&db, &other).0.as_deref(), Some("merge_conflict"));
}
