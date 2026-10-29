use super::*;
use crate::work::WorkerFailureBasis;
use boss_protocol::{CreateRevisionInput, OperatorAnswer, RequestExecutionInput};

fn latest_ledger(db: &WorkDb, work_item_id: &str, property: &str) -> (String, String) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT basis, actor FROM task_status_audit
             WHERE work_item_id = ?1 AND property = ?2
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![work_item_id, property],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn insert_ledger(db: &WorkDb, work_item_id: &str, property: &str) -> (String, String) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT basis, actor FROM task_status_audit
             WHERE work_item_id = ?1 AND property = ?2 AND old_value IS NULL
             ORDER BY id ASC LIMIT 1",
            rusqlite::params![work_item_id, property],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn start_waiting_human(db: &WorkDb, work_item_id: &str) -> crate::work::WorkExecution {
    let exec = create_ready_chore_execution(db, work_item_id.to_string());
    let (exec, run) = db
        .start_execution_run(&exec.id, "worker-1", "mono", "lease-1", "mono-agent-001", "/tmp/ws")
        .unwrap();
    crate::test_support::finish_run_worker_pane_alive(db, &exec.id, &run.id, Some("spawned"));
    exec
}

#[tokio::test]
async fn completion_producers_carry_their_basis_to_the_failure_transaction() {
    for producer in [
        WorkerFailureBasis::DeclaredBlocked,
        WorkerFailureBasis::NudgeBreaker,
        WorkerFailureBasis::DeliveredWithoutPr,
        WorkerFailureBasis::ForeignPrBinding,
    ] {
        let workspace = tempdir().unwrap();
        let (_dir, db, product, chore, execution) = fixture(workspace.path());
        let foreign_pr = "https://github.com/spinyfin/mono/pull/123";
        let detector = if producer == WorkerFailureBasis::ForeignPrBinding {
            let owner = create_test_chore(&db, product, "PR owner");
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET pr_url = ?2, status = 'done' WHERE id = ?1",
                    rusqlite::params![owner.id, foreign_pr],
                )
                .unwrap();
            StubPrDetector::ok(Some(foreign_pr))
        } else {
            StubPrDetector::ok(None)
        };
        let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
        let handler = handler.with_max_unproductive_nudges(2);
        let outcome = match producer {
            WorkerFailureBasis::DeclaredBlocked => {
                handler
                    .finalize_declared_run_done(&execution, boss_protocol::RunDoneOutcome::Blocked)
                    .await
            }
            WorkerFailureBasis::DeliveredWithoutPr => {
                handler
                    .finalize_declared_run_done(&execution, boss_protocol::RunDoneOutcome::Delivered)
                    .await
            }
            WorkerFailureBasis::NudgeBreaker => {
                let first = handler.on_stop(&execution).await;
                let second = handler.on_stop(&execution).await;
                assert!(
                    matches!(first, StopOutcome::AwaitingInput),
                    "first Stop is the produce-a-PR nudge, got {first:?}"
                );
                assert!(
                    matches!(second, StopOutcome::AwaitingInput),
                    "second Stop is still the produce-a-PR nudge, got {second:?}"
                );
                handler.on_stop(&execution).await
            }
            WorkerFailureBasis::ForeignPrBinding => handler.on_stop(&execution).await,
            _ => unreachable!(),
        };
        assert!(
            !matches!(outcome, StopOutcome::DbError),
            "{}: {outcome:?}",
            producer.as_str()
        );
        let (basis, actor) = latest_ledger(&db, &chore, "blocked_reason");
        assert_eq!(basis, producer.as_str());
        assert_eq!(actor, "engine");
    }
}

#[test]
fn operator_declined_answer_records_operator_no() {
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Needs a decision");
    let execution = start_waiting_human(&db, &chore.id);
    let question = boss_protocol::OperatorQuestion {
        text: "Ship the change?".into(),
        explanation: "The worker is blocked on a product choice.".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
    };
    crate::work::insert_question(&db.connect().unwrap(), &execution.id, "proposal", &question).unwrap();
    db.record_worker_awaiting_operator_answer(&execution.id, "needs a decision", &question)
        .unwrap();
    db.answer_operator_question(&chore.id, OperatorAnswer::YesNo { value: false })
        .unwrap();
    let (basis, actor) = latest_ledger(&db, &chore.id, "blocked_reason");
    assert_eq!(basis, WorkerFailureBasis::OperatorNo.as_str());
    assert_eq!(actor, "human");
}

#[test]
fn create_revision_records_caller_actor() {
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let product = create_test_product(&db);
    let parent = create_test_chore(&db, product.id, "Parent");
    let pr_url = "https://github.com/spinyfin/mono/pull/42";
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![parent.id, pr_url],
        )
        .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let engine = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&parent.id)
                .description("Engine-minted revision")
                .build(),
            &checker,
        )
        .unwrap();
    let (basis, actor) = insert_ledger(&db, &engine.id, "status");
    assert_eq!(basis, "insert_revision_in_tx");
    assert_eq!(actor, "engine");

    let human = db
        .create_revision_as_actor(
            CreateRevisionInput::builder()
                .parent_task_id(&parent.id)
                .description("Human-filed revision")
                .build(),
            &checker,
            "human",
        )
        .unwrap();
    let (basis, actor) = insert_ledger(&db, &human.id, "status");
    assert_eq!(basis, "insert_revision_in_tx");
    assert_eq!(actor, "human");
}

#[test]
fn bounce_and_explicit_start_clears_record_exact_bases() {
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let product = create_test_product(&db);

    let bounced = create_test_chore(&db, product.id.clone(), "Bounced");
    db.bounce_dispatch_failed_to_backlog(&bounced.id, "cube_workspace_lease_failed", "lease failed")
        .unwrap();
    let (basis, actor) = latest_ledger(&db, &bounced.id, "dispatch_failed_reason");
    assert_eq!(basis, "cube_workspace_lease_failed");
    assert_eq!(actor, "engine");
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(&bounced.id)
            .explicit_start(true)
            .build(),
    )
    .unwrap();
    let (basis, actor) = latest_ledger(&db, &bounced.id, "dispatch_failed_reason");
    assert_eq!(basis, "explicit_start_dispatch_failure_clear");
    assert_eq!(actor, "engine");

    let failed = create_test_chore(&db, product.id.clone(), "Failed worker");
    let exec = start_waiting_human(&db, &failed.id);
    db.record_worker_failure_with_basis(&exec.id, "breaker tripped", WorkerFailureBasis::NudgeBreaker)
        .unwrap();
    let (basis, actor) = latest_ledger(&db, &failed.id, "blocked_reason");
    assert_eq!(basis, "nudge_breaker");
    assert_eq!(actor, "engine");

    let unverified = create_test_chore(&db, product.id.clone(), "Conflict delivery");
    let unverified_exec = start_waiting_human(&db, &unverified.id);
    db.record_worker_failure_with_basis(
        &unverified_exec.id,
        "conflict delivery unverified",
        WorkerFailureBasis::ConflictDeliveryUnverified,
    )
    .unwrap();
    let (basis, actor) = latest_ledger(&db, &unverified.id, "blocked_reason");
    assert_eq!(basis, "conflict_delivery_unverified");
    assert_eq!(actor, "engine");
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(&failed.id)
            .explicit_start(true)
            .build(),
    )
    .unwrap();
    let (basis, actor) = latest_ledger(&db, &failed.id, "blocked_reason");
    assert_eq!(basis, "explicit_start_failure_clear");
    assert_eq!(actor, "engine");
}

#[test]
fn merge_conflict_and_dependency_producers_record_their_basis() {
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Conflicted");
    let pr_url = "https://github.com/spinyfin/mono/pull/77";
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore.id, pr_url],
        )
        .unwrap();
    db.mark_chore_blocked_merge_conflict(&chore.id, pr_url).unwrap();
    let (basis, actor) = latest_ledger(&db, &chore.id, "blocked_reason");
    assert_eq!(basis, "mark_chore_blocked_merge_conflict");
    assert_eq!(actor, "engine");

    let prereq = create_test_chore(&db, product.id.clone(), "Prereq");
    let dependent = create_test_chore(&db, product.id, "Dependent");
    db.add_dependency(boss_protocol::AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: prereq.id,
        relation: None,
    })
    .unwrap();
    let (basis, actor) = latest_ledger(&db, &dependent.id, "blocked_reason");
    assert_eq!(basis, "maybe_engine_block_dependent");
    assert_eq!(actor, "engine");
}
