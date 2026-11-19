use super::*;

// An answer on a conflict revision whose spawning attempt has retired: the
// explicit start reopens the attempt, and the same-chain revision gated on it
// must stay gated throughout. A suppressed answer leaves the row alone for a
// later explicit start instead of settling it blocked.

fn blocked_fields(db: &WorkDb, task_id: &str) -> (Option<String>, Option<String>) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT blocked_reason, blocked_detail FROM tasks WHERE id = ?1",
            rusqlite::params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

struct RetiredConflictRevision {
    db: WorkDb,
    product_id: String,
    revision_id: String,
    attempt_id: String,
    dependent_id: String,
}

fn retired_conflict_revision_awaiting_answer(label: &str) -> RetiredConflictRevision {
    let db = WorkDb::open(temp_db_path(label)).unwrap();
    let product_id = make_revision_product(&db, label);
    let pr_url = "https://github.com/spinyfin/mono/pull/3160";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 3160,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &attempt.id);
    db.set_conflict_resolution_revision_task_id(&attempt.id, &revision_id)
        .unwrap();
    db.reconcile_product_executions(&product_id).unwrap();
    let first = executions_for(&db, &revision_id)[0].0.clone();
    db.start_execution_run(
        &first,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
    db.mark_conflict_resolution_failed(&attempt.id, "resolver gave up")
        .unwrap();
    let question = boss_protocol::OperatorQuestion {
        text: "Force the merge?".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
        explanation: "The resolver could not finish.".into(),
    };
    crate::pr_flow::insert_question(&db.connect().unwrap(), &first, "prop_retired", &question).unwrap();
    db.record_worker_awaiting_operator_answer(&first, "needs a decision", &question)
        .unwrap()
        .expect("live execution parks the revision");

    // A PR-review revision on the same chain is gated on the conflict revision.
    let dependent_id = insert_revision_row(&db, &product_id, &chore_id);
    {
        let conn = db.connect().unwrap();
        deps::insert_edge(&conn, &dependent_id, &revision_id, RELATION_BLOCKS, &now_string()).unwrap();
        let mut pending = PendingEvents::default();
        maybe_engine_block_dependent(&mut pending, &conn, &dependent_id, &now_string()).unwrap();
    }
    assert_eq!(task_status(&db, &dependent_id), "blocked");
    RetiredConflictRevision {
        db,
        product_id,
        revision_id,
        attempt_id: attempt.id,
        dependent_id,
    }
}

fn assert_dependent_still_gated(fixture: &RetiredConflictRevision) {
    let db = &fixture.db;
    assert_eq!(task_status(db, &fixture.dependent_id), "blocked");
    assert_eq!(
        blocked_fields(db, &fixture.dependent_id).0.as_deref(),
        Some("dependency")
    );
    let conn = db.connect().unwrap();
    let gating = deps::gating_prereqs_for(&conn, &fixture.dependent_id).unwrap();
    assert_eq!(gating, vec![fixture.revision_id.clone()]);
}

#[test]
fn answering_a_retired_attempt_revision_starts_it_without_releasing_its_dependents() {
    let fixture = retired_conflict_revision_awaiting_answer("answer-retired-start");
    let db = &fixture.db;
    let outcome = db
        .answer_operator_question(
            &fixture.revision_id,
            boss_protocol::OperatorAnswer::YesNo { value: true },
        )
        .unwrap();
    assert!(
        outcome.minted_execution,
        "the default answer requests an explicit start"
    );
    assert_eq!(
        db.get_conflict_resolution(&fixture.attempt_id).unwrap().unwrap().status,
        "pending"
    );
    assert_eq!(task_status(db, &fixture.revision_id), "todo");
    assert_eq!(blocked_fields(db, &fixture.revision_id), (None, None));
    assert_dependent_still_gated(&fixture);

    db.reconcile_product_executions(&fixture.product_id).unwrap();
    let live: Vec<_> = executions_for(db, &fixture.revision_id)
        .into_iter()
        .filter(|(_, status)| status == "ready")
        .collect();
    assert_eq!(live.len(), 1, "exactly one live continuation: {live:?}");
    assert_eq!(task_status(db, &fixture.revision_id), "todo");
    assert_dependent_still_gated(&fixture);
}

#[test]
fn suppressed_answer_on_a_retired_attempt_revision_requests_no_start_and_settles_nothing() {
    let fixture = retired_conflict_revision_awaiting_answer("answer-retired-suppressed");
    let db = &fixture.db;
    let outcome = db
        .answer_operator_question_from(
            &fixture.revision_id,
            boss_protocol::OperatorAnswer::YesNo { value: true },
            &GhPrStateChecker,
            boss_protocol::LAST_STATUS_ACTOR_HUMAN,
            None,
            true,
        )
        .unwrap();
    assert!(
        !outcome.minted_execution,
        "a suppressed answer requests no explicit start"
    );
    assert_eq!(
        db.get_conflict_resolution(&fixture.attempt_id).unwrap().unwrap().status,
        "failed",
        "only an explicit start reopens a retired attempt"
    );
    assert_eq!(task_status(db, &fixture.revision_id), "todo");
    assert_dependent_still_gated(&fixture);
}

#[test]
fn suppressed_answer_from_a_force_launched_asker_on_a_retired_attempt_revision_names_no_launch() {
    let fixture = retired_conflict_revision_awaiting_answer("answer-retired-force-launched");
    let db = &fixture.db;
    let asking = executions_for(db, &fixture.revision_id)[0].0.clone();
    db.mark_execution_force_launched(&asking).unwrap();
    let outcome = db
        .answer_operator_question_from(
            &fixture.revision_id,
            boss_protocol::OperatorAnswer::YesNo { value: true },
            &GhPrStateChecker,
            boss_protocol::LAST_STATUS_ACTOR_HUMAN,
            None,
            true,
        )
        .unwrap();
    assert!(!outcome.minted_execution);
    assert_eq!(
        outcome.dispatch_work_item_id, None,
        "no continuation exists, so there is nothing to launch"
    );
    let executions = executions_for(db, &fixture.revision_id);
    assert_eq!(executions.len(), 1, "no continuation is queued: {executions:?}");
    assert!(!db.execution_has_operator_forced_admission(&asking).unwrap());
    assert_dependent_still_gated(&fixture);
}
