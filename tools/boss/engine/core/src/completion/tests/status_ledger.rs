use super::*;
use crate::merge_poller::OpenPrMergeability;
use crate::work::{
    AddDependencyInput, CREATED_VIA_CI_FIX_PREFIX, CREATED_VIA_PR_REVIEW_PREFIX, CiRemediationInsertInput,
    ConflictResolutionInsertInput, CreateExecutionInput, CreateProjectInput, CreateRevisionInput, CreateTaskInput,
    ExecutionKind, ExecutionStatus, FakePrStateChecker, LAST_STATUS_ACTOR_HUMAN, PrOpenState, RequestExecutionInput,
    ReviewBatchCreateInput, ReviewBatchMemberCreateInput, ReviewBatchMemberRole, ReviewBatchMemberStatus,
    ReviewBatchPhase, ReviewClassification, SubmitWorkerProposalInput, WorkItemPatch, WorkerFailureBasis,
    WorkerPrCompletionTarget,
};
use boss_protocol::{
    CreateInvestigationInput, OperatorAnswer, OperatorAnswerType, OperatorQuestion, ProposalKind, ReviewLanguageBucket,
    ReviewProfile,
};

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

fn assert_latest(db: &WorkDb, work_item_id: &str, property: &str, basis: &str, actor: &str) {
    assert_eq!(
        latest_ledger(db, work_item_id, property),
        (basis.to_owned(), actor.to_owned())
    );
}

fn assert_has(db: &WorkDb, work_item_id: &str, property: &str, basis: &str, actor: &str) {
    let row: (String, String) = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT basis, actor FROM task_status_audit
             WHERE work_item_id = ?1 AND property = ?2 AND basis = ?3
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![work_item_id, property, basis],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, (basis.to_owned(), actor.to_owned()));
}

fn set_pr_url(db: &WorkDb, work_item_id: &str, pr_url: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![work_item_id, pr_url],
        )
        .unwrap();
}

fn blocked_detail(db: &WorkDb, work_item_id: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row("SELECT blocked_detail FROM tasks WHERE id = ?1", [work_item_id], |r| {
            r.get(0)
        })
        .unwrap()
}

fn bind_pr(db: &WorkDb, work_item_id: &str, pr_url: &str) {
    db.update_work_item(
        work_item_id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..Default::default()
        },
    )
    .unwrap();
}

fn memory_db() -> (WorkDb, String) {
    let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
    let product = create_test_product(&db);
    (db, product.id)
}

fn ci_input(product_id: &str, work_item_id: &str, pr_url: &str) -> CiRemediationInsertInput {
    CiRemediationInsertInput {
        product_id: product_id.to_owned(),
        work_item_id: work_item_id.to_owned(),
        pr_url: pr_url.to_owned(),
        pr_number: 9,
        head_branch: "feat".into(),
        head_sha_at_trigger: "abc".into(),
        attempt_kind: "fix".into(),
        consumes_budget: 1,
        failed_checks: "[]".into(),
        failure_kind: "pr_branch_ci".into(),
        before_commit_sha: None,
    }
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

#[test]
fn worker_failure_bases_each_record_a_repeat_observation() {
    let (db, product) = memory_db();
    assert_eq!(
        WorkerFailureBasis::all(),
        &[
            WorkerFailureBasis::NudgeBreaker,
            WorkerFailureBasis::DeclaredBlocked,
            WorkerFailureBasis::DeliveredWithoutPr,
            WorkerFailureBasis::ForeignPrBinding,
            WorkerFailureBasis::OperatorNo,
            WorkerFailureBasis::ConflictDeliveryUnverified,
        ]
    );
    let repeat_on_same_reason = [
        WorkerFailureBasis::NudgeBreaker,
        WorkerFailureBasis::DeclaredBlocked,
        WorkerFailureBasis::DeliveredWithoutPr,
        WorkerFailureBasis::ForeignPrBinding,
        WorkerFailureBasis::OperatorNo,
    ];
    for producer in repeat_on_same_reason {
        let chore = create_test_chore(&db, product.clone(), producer.as_str());
        let exec = start_waiting_human(&db, &chore.id);
        db.record_worker_failure_with_basis(&exec.id, "first failure", WorkerFailureBasis::NudgeBreaker)
            .unwrap();
        let first_id: i64 = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT id FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason'
                 ORDER BY id DESC LIMIT 1",
                [&chore.id],
                |r| r.get(0),
            )
            .unwrap();
        let second_exec = start_waiting_human(&db, &chore.id);
        db.record_worker_failure_with_basis(&second_exec.id, "repeat", producer)
            .unwrap();
        assert_latest(&db, &chore.id, "blocked_reason", producer.as_str(), "engine");
        let second_id: i64 = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT id FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason'
                 ORDER BY id DESC LIMIT 1",
                [&chore.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(second_id > first_id, "{}", producer.as_str());
    }

    let unverified = create_test_chore(&db, product, WorkerFailureBasis::ConflictDeliveryUnverified.as_str());
    let exec = start_waiting_human(&db, &unverified.id);
    db.record_worker_failure_with_basis(&exec.id, "first failure", WorkerFailureBasis::NudgeBreaker)
        .unwrap();
    let first_id: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT id FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason'
             ORDER BY id DESC LIMIT 1",
            [&unverified.id],
            |r| r.get(0),
        )
        .unwrap();
    let second_exec = start_waiting_human(&db, &unverified.id);
    db.record_worker_failure_with_basis(
        &second_exec.id,
        "repeat",
        WorkerFailureBasis::ConflictDeliveryUnverified,
    )
    .unwrap();
    assert_latest(&db, &unverified.id, "blocked_reason", "nudge_breaker", "engine");
    let second_id: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT id FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason'
             ORDER BY id DESC LIMIT 1",
            [&unverified.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(second_id, first_id);
}

#[test]
fn empty_detail_fold_keeps_existing_worker_failed_detail() {
    let (db, product) = memory_db();
    let chore = create_test_chore(&db, product, "Fold");
    let exec = start_waiting_human(&db, &chore.id);
    db.record_worker_failure_with_basis(&exec.id, "kept detail", WorkerFailureBasis::DeclaredBlocked)
        .unwrap();
    let second = start_waiting_human(&db, &chore.id);
    db.record_worker_failure_with_basis(&second.id, "   ", WorkerFailureBasis::NudgeBreaker)
        .unwrap();
    assert_eq!(blocked_detail(&db, &chore.id).as_deref(), Some("kept detail"));
    assert_latest(&db, &chore.id, "blocked_reason", "nudge_breaker", "engine");
}

#[test]
fn explicit_start_clears_record_exact_bases_on_all_five_paths() {
    let (db, product) = memory_db();

    let bounced = create_test_chore(&db, product.clone(), "Dispatch clear");
    db.bounce_dispatch_failed_to_backlog(&bounced.id, "cube_workspace_lease_failed", "lease failed")
        .unwrap();
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(&bounced.id)
            .explicit_start(true)
            .build(),
    )
    .unwrap();
    assert_latest(
        &db,
        &bounced.id,
        "dispatch_failed_reason",
        "explicit_start_dispatch_failure_clear",
        "engine",
    );

    let failed = create_test_chore(&db, product.clone(), "Failure clear");
    let exec = start_waiting_human(&db, &failed.id);
    db.record_worker_failure_with_basis(&exec.id, "breaker", WorkerFailureBasis::NudgeBreaker)
        .unwrap();
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(&failed.id)
            .explicit_start(true)
            .build(),
    )
    .unwrap();
    assert_latest(
        &db,
        &failed.id,
        "blocked_reason",
        "explicit_start_failure_clear",
        "engine",
    );

    let stale_dep = create_test_chore(&db, product.clone(), "Stale dependency");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'dependency' WHERE id = ?1",
            rusqlite::params![stale_dep.id],
        )
        .unwrap();
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(&stale_dep.id)
            .explicit_start(true)
            .build(),
    )
    .unwrap();
    assert_latest(
        &db,
        &stale_dep.id,
        "blocked_reason",
        "explicit_start_dependency_clear",
        "engine",
    );

    let pr = "https://github.com/spinyfin/mono/pull/88";
    let parent = create_test_chore(&db, product.clone(), "Conflict parent");
    bind_pr(&db, &parent.id, pr);
    db.mark_chore_blocked_merge_conflict(&parent.id, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: parent.id.clone(),
            pr_url: pr.into(),
            pr_number: 88,
            head_branch: "feat".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base".into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap()
        .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&parent.id)
                .description("Resolve conflict")
                .created_via(format!("merge-conflict:{}", attempt.id))
                .build(),
            &checker,
        )
        .unwrap();
    db.mark_conflict_resolution_failed(&attempt.id, "gave up").unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'spawning_attempt_retired_unsuccessfully'
             WHERE id = ?1",
            rusqlite::params![revision.id],
        )
        .unwrap();
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(&revision.id)
            .explicit_start(true)
            .build(),
    )
    .unwrap();
    assert_latest(
        &db,
        &revision.id,
        "blocked_reason",
        "explicit_start_retired_attempt_clear",
        "engine",
    );

    let ci_parent = create_test_chore(&db, product.clone(), "CI parent");
    bind_pr(&db, &ci_parent.id, pr);
    db.mark_chore_blocked_ci_failure(&ci_parent.id, pr, None).unwrap();
    let ci_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&ci_parent.id)
                .kind(ExecutionKind::CiRemediation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(&ci_exec.id, "worker-ci", "mono", "lease-ci", "ws-ci", "/tmp/ci")
        .unwrap();
    db.request_execution_with_live_check(
        RequestExecutionInput::builder()
            .work_item_id(&ci_parent.id)
            .explicit_start(true)
            .build(),
        |_| false,
    )
    .unwrap();
    assert_latest(
        &db,
        &ci_parent.id,
        "blocked_reason",
        "explicit_start_ci_failure_clear",
        "engine",
    );
}

#[test]
fn explicit_start_clear_then_fail_does_not_reuse_old_provenance() {
    let (db, product) = memory_db();
    let chore = create_test_chore(&db, product, "Clear then fail");
    let exec = start_waiting_human(&db, &chore.id);
    db.record_worker_failure_with_basis(&exec.id, "first", WorkerFailureBasis::DeclaredBlocked)
        .unwrap();
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(&chore.id)
            .explicit_start(true)
            .build(),
    )
    .unwrap();
    assert_latest(
        &db,
        &chore.id,
        "blocked_reason",
        "explicit_start_failure_clear",
        "engine",
    );
    let next = start_waiting_human(&db, &chore.id);
    db.record_worker_failure_with_basis(&next.id, "second", WorkerFailureBasis::NudgeBreaker)
        .unwrap();
    assert_latest(&db, &chore.id, "blocked_reason", "nudge_breaker", "engine");
}

#[tokio::test]
async fn conflict_delivery_gate_records_conflict_delivery_unverified() {
    let workspace = tempdir().unwrap();
    let pr = "https://github.com/spinyfin/mono/pull/2070";
    let (_dir, db, _, _, revision, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_merge_probe(Arc::new(FixedMergeabilityProbe {
            mergeability: OpenPrMergeability::Conflict,
        }));
    let execution = db.get_execution(&execution_id).unwrap();
    let outcome = handler.conflict_delivery_decision(&execution, Some(pr)).await;
    assert!(
        matches!(outcome, super::super::ConflictDeliveryDecision::Failed(_)),
        "{outcome:?}"
    );
    assert_latest(
        &db,
        &revision,
        "blocked_reason",
        WorkerFailureBasis::ConflictDeliveryUnverified.as_str(),
        "engine",
    );

    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, revision, execution_id, _) = conflict_revision_fixture(workspace.path(), pr, "old-head");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None)).handler;
    let execution = db.get_execution(&execution_id).unwrap();
    handler.conflict_delivery_decision(&execution, None).await;
    assert_latest(
        &db,
        &revision,
        "blocked_reason",
        WorkerFailureBasis::ConflictDeliveryUnverified.as_str(),
        "engine",
    );
}

#[test]
fn blocking_producers_record_exact_bases() {
    let (db, product) = memory_db();
    let pr = "https://github.com/spinyfin/mono/pull/101";
    let chore = create_test_chore(&db, product.clone(), "Blocking");
    bind_pr(&db, &chore.id, pr);

    db.mark_chore_blocked_ci_failure(&chore.id, pr, None).unwrap();
    assert_latest(
        &db,
        &chore.id,
        "blocked_reason",
        "mark_chore_blocked_ci_signal",
        "engine",
    );
    db.mark_chore_blocked_ci_failure_exhausted(&chore.id, pr).unwrap();
    assert_latest(
        &db,
        &chore.id,
        "blocked_reason",
        "mark_chore_blocked_ci_failure_exhausted",
        "engine",
    );
    db.clear_chore_blocked_ci_failure(&chore.id, pr).unwrap();
    assert_latest(
        &db,
        &chore.id,
        "blocked_reason",
        "clear_chore_blocked_ci_failure",
        "engine",
    );

    db.mark_chore_blocked_ci_failure(&chore.id, pr, None).unwrap();
    db.retarget_blocked_ci_failure_to_merge_conflict(&chore.id, pr).unwrap();
    assert_latest(
        &db,
        &chore.id,
        "blocked_reason",
        "retarget_blocked_ci_failure_to_merge_conflict",
        "engine",
    );
    db.clear_chore_blocked_merge_conflict(&chore.id, pr).unwrap();
    assert_latest(
        &db,
        &chore.id,
        "blocked_reason",
        "clear_chore_blocked_merge_conflict",
        "engine",
    );

    db.mark_chore_blocked_merge_conflict(&chore.id, pr).unwrap();
    assert_latest(
        &db,
        &chore.id,
        "blocked_reason",
        "mark_chore_blocked_merge_conflict",
        "engine",
    );
    db.mark_chore_blocked_deletion_signoff(&chore.id, pr).unwrap();
    assert_latest(
        &db,
        &chore.id,
        "blocked_reason",
        "mark_chore_blocked_deletion_signoff",
        "engine",
    );

    let stranded = create_test_chore(&db, product.clone(), "Stranded");
    bind_pr(&db, &stranded.id, pr);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = NULL WHERE id = ?1",
            rusqlite::params![stranded.id],
        )
        .unwrap();
    db.recanonicalize_blocked_merge_conflict(&stranded.id, pr).unwrap();
    assert_latest(
        &db,
        &stranded.id,
        "blocked_reason",
        "recanonicalize_blocked_signal",
        "engine",
    );

    let retry = create_test_chore(&db, product.clone(), "CI retry");
    bind_pr(&db, &retry.id, pr);
    db.mark_chore_blocked_ci_failure_exhausted(&retry.id, pr).unwrap();
    db.retry_ci_remediation_for_work_item(&retry.id).unwrap();
    assert_latest(
        &db,
        &retry.id,
        "blocked_reason",
        "retry_ci_remediation_for_work_item",
        "engine",
    );

    let green = create_test_chore(&db, product.clone(), "Validated green");
    bind_pr(&db, &green.id, pr);
    db.mark_chore_blocked_ci_failure(&green.id, pr, None).unwrap();
    let attempt = db
        .insert_ci_remediation(ci_input(&product, &green.id, pr))
        .unwrap()
        .unwrap();
    db.mark_ci_remediation_validated_green(&attempt.id, Some("def"))
        .unwrap();
    assert_latest(
        &db,
        &green.id,
        "blocked_reason",
        "mark_ci_remediation_validated_green",
        "engine",
    );

    let attempt_clear = create_test_chore(&db, product, "Attempt clear");
    bind_pr(&db, &attempt_clear.id, pr);
    db.mark_chore_blocked_merge_conflict(&attempt_clear.id, pr).unwrap();
    let conflict = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: attempt_clear.product_id.clone(),
            work_item_id: attempt_clear.id.clone(),
            pr_url: pr.into(),
            pr_number: 101,
            head_branch: "feat".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base".into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap()
        .unwrap();
    db.clear_chore_blocked_merge_conflict_for_attempt(&attempt_clear.id, pr, &conflict.id)
        .unwrap();
    assert_latest(
        &db,
        &attempt_clear.id,
        "blocked_reason",
        "clear_chore_blocked_merge_conflict_for_attempt",
        "engine",
    );
}

#[test]
fn dispatch_and_execution_producers_record_exact_bases() {
    let (db, product) = memory_db();
    let ghost = create_test_chore(&db, product.clone(), "Ghost");
    db.update_work_item(
        &ghost.id,
        WorkItemPatch {
            status: Some("active".into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.heal_ghost_active_chores().unwrap();
    assert_latest(&db, &ghost.id, "status", "heal_ghost_active_chores", "engine");

    let demote = create_test_chore(&db, product.clone(), "Demote");
    db.update_work_item(
        &demote.id,
        WorkItemPatch {
            status: Some("active".into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.demote_active_work_item_to_todo(&demote.id).unwrap();
    assert_latest(&db, &demote.id, "status", "demote_active_work_item_to_todo", "engine");

    let parked = create_test_chore(&db, product.clone(), "Parked");
    db.bounce_dispatch_failed_to_backlog(&parked.id, "lease_failed", "lease")
        .unwrap();
    assert_latest(&db, &parked.id, "dispatch_failed_reason", "lease_failed", "engine");
    db.reenable_and_request_execution_with_live_check(
        &parked.id,
        RequestExecutionInput::builder().work_item_id(&parked.id).build(),
        |_| true,
    )
    .unwrap();
    assert_latest(
        &db,
        &parked.id,
        "autostart",
        "reenable_autostart_after_dispatch_failure_in_tx",
        "engine",
    );

    let started = create_test_chore(&db, product.clone(), "Start run");
    let exec = create_ready_chore_execution(&db, started.id.clone());
    db.start_execution_run(&exec.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert_latest(
        &db,
        &started.id,
        "status",
        "start_execution_run_on_host_with_tmux_hosting",
        "engine",
    );

    let cancel = create_test_chore(&db, product.clone(), "Cancel");
    let cancel_exec = start_waiting_human(&db, &cancel.id);
    db.cancel_execution(&cancel_exec.id).unwrap();
    assert_latest(&db, &cancel.id, "status", "demote_active_if_latest_execution", "engine");

    let orphan = create_test_chore(&db, product.clone(), "Readopt");
    let orphan_exec = start_waiting_human(&db, &orphan.id);
    db.demote_active_work_item_to_todo(&orphan.id).unwrap();
    db.mark_execution_orphaned(&orphan_exec.id, "pane gone").unwrap();
    db.readopt_inferred_terminal_execution(&orphan_exec.id, "hooks alive", Some(1))
        .unwrap();
    assert_latest(
        &db,
        &orphan.id,
        "status",
        "readopt_inferred_terminal_execution",
        "engine",
    );

    let banner = create_test_chore(&db, product.clone(), "Stale banner");
    let banner_exec = create_ready_chore_execution(&db, banner.id.clone());
    db.start_execution_run(&banner_exec.id, "w", "mono", "l", "ws", "/tmp/ws")
        .unwrap();
    db.bounce_dispatch_failed_to_backlog(&banner.id, "lease_failed", "lease")
        .unwrap();
    db.reconcile_stale_attention_signals().unwrap();
    assert_latest(
        &db,
        &banner.id,
        "dispatch_failed_reason",
        "clear_stale_dispatch_failures",
        "engine",
    );

    let revision_parent = create_test_chore(&db, product, "Stale revision parent");
    let pr = "https://github.com/spinyfin/mono/pull/102";
    bind_pr(&db, &revision_parent.id, pr);
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&revision_parent.id)
                .description("Stale")
                .build(),
            &checker,
        )
        .unwrap();
    let ready = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&revision.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.retire_stale_revision_before_dispatch(&ready.id, &revision.id)
        .unwrap();
    assert_latest(
        &db,
        &revision.id,
        "status",
        "retire_stale_revision_before_dispatch",
        "engine",
    );
}

#[test]
fn pr_flow_and_binding_producers_record_exact_bases() {
    let (db, product) = memory_db();
    let pr = "https://github.com/spinyfin/mono/pull/103";
    let chore = create_test_chore(&db, product.clone(), "PR flow");
    let exec = start_waiting_human(&db, &chore.id);
    db.record_worker_pr_completion(
        &exec.id,
        pr,
        Some("abc"),
        None,
        WorkerPrCompletionTarget::InReview,
        None,
    )
    .unwrap();
    assert_latest(&db, &chore.id, "status", "record_worker_pr_completion", "engine");

    let noop = create_test_chore(&db, product.clone(), "No-op");
    let noop_exec = start_waiting_human(&db, &noop.id);
    db.record_worker_no_op_completion(&noop_exec.id, "already on main", None)
        .unwrap();
    assert_latest(
        &db,
        &noop.id,
        "status",
        "record_worker_no_op_completion_inner",
        "engine",
    );

    let merged = create_test_chore(&db, product.clone(), "Merged");
    bind_pr(&db, &merged.id, pr);
    db.mark_chore_pr_merged(&merged.id, pr).unwrap();
    assert_latest(&db, &merged.id, "status", "mark_chore_pr_merged", "engine");

    let closed = create_test_chore(&db, product.clone(), "Closed");
    bind_pr(&db, &closed.id, pr);
    db.mark_chore_pr_closed_unmerged(&closed.id, pr).unwrap();
    assert_latest(&db, &closed.id, "status", "mark_chore_pr_closed_unmerged", "engine");

    let bind_active = create_test_chore(&db, product.clone(), "Bind active");
    db.update_work_item(
        &bind_active.id,
        WorkItemPatch {
            status: Some("active".into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.bind_pr_to_active_task_from_terminal_execution(&bind_active.id, pr)
        .unwrap();
    assert_latest(
        &db,
        &bind_active.id,
        "status",
        "bind_pr_to_active_task_from_terminal_execution",
        "engine",
    );

    let bind_terminal = create_test_chore(&db, product.clone(), "Bind terminal");
    db.bind_pr_to_task_from_terminal_execution(&bind_terminal.id, pr)
        .unwrap();
    assert_latest(
        &db,
        &bind_terminal.id,
        "status",
        "bind_pr_to_task_from_terminal_execution",
        "engine",
    );

    let held = create_test_chore(&db, product.clone(), "Held review");
    db.update_work_item(
        &held.id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(pr.into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.advance_held_pending_review_task_to_in_review(&held.id).unwrap();
    assert_latest(
        &db,
        &held.id,
        "status",
        "advance_held_pending_review_task_to_in_review_in_tx",
        "engine",
    );

    let rebound = create_test_chore(&db, product.clone(), "Rebind");
    set_pr_url(&db, &rebound.id, pr);
    let rebound_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&rebound.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    db.rebind_task_pr_from_execution_branch(
        &rebound.id,
        &rebound_exec.id,
        pr,
        "https://github.com/spinyfin/mono/pull/104",
    )
    .unwrap();
    assert_latest(
        &db,
        &rebound.id,
        "status",
        "rebind_task_pr_from_execution_branch",
        "engine",
    );

    let unresolved = create_test_chore(&db, product.clone(), "Unresolved binding");
    bind_pr(&db, &unresolved.id, pr);
    let unresolved_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&unresolved.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    db.mark_task_pr_binding_unresolved(&unresolved.id, &unresolved_exec.id, pr, "no PR on branch")
        .unwrap();
    assert_latest(
        &db,
        &unresolved.id,
        "blocked_reason",
        "mark_task_pr_binding_unresolved",
        "engine",
    );
    assert_eq!(
        latest_ledger(&db, &unresolved.id, "status").0,
        "mark_task_pr_binding_unresolved"
    );

    let close = create_test_chore(&db, product, "Reconciler close");
    db.reconciler_close_work_item(&close.id, &[]).unwrap();
    assert_latest(&db, &close.id, "status", "reconciler_close_work_item", "engine");
}

#[test]
fn insert_and_update_producers_record_exact_bases() {
    let (db, product) = memory_db();
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(&product)
                .name("Ledger project")
                .build(),
        )
        .unwrap();
    let design_id: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&project.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        insert_ledger(&db, &design_id, "status").0,
        "insert_design_task_for_project_in_tx"
    );
    assert_eq!(insert_ledger(&db, &design_id, "status").1, "engine");

    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(&product)
                .project_id(&project.id)
                .name("Project task")
                .build(),
        )
        .unwrap();
    assert_eq!(
        insert_ledger(&db, &task.id, "status"),
        ("insert_task_in_tx_as_actor".into(), "engine".into())
    );

    let investigation = db
        .create_investigation(
            CreateInvestigationInput::builder()
                .product_id(&product)
                .name("Investigate")
                .build(),
        )
        .unwrap();
    assert_eq!(
        insert_ledger(&db, &investigation.id, "status"),
        ("insert_investigation_in_tx_as_actor".into(), "engine".into())
    );

    let chore = create_test_chore(&db, product.clone(), "Insert chore");
    assert_eq!(
        insert_ledger(&db, &chore.id, "status"),
        ("insert_chore_with_project_in_tx_as_actor".into(), "engine".into())
    );

    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
            ..Default::default()
        },
        LAST_STATUS_ACTOR_HUMAN,
    )
    .unwrap();
    assert_latest(&db, &chore.id, "status", "update_task", "human");
}

#[test]
fn chain_conflict_and_queue_producers_record_exact_bases() {
    let (db, product) = memory_db();
    let pr = "https://github.com/spinyfin/mono/pull/105";
    let parent = create_test_chore(&db, product.clone(), "Chain parent");
    bind_pr(&db, &parent.id, pr);
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let in_review_rev = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&parent.id)
                .description("In review revision")
                .build(),
            &checker,
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![in_review_rev.id],
        )
        .unwrap();
    let moot = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&parent.id)
                .description("Moot CI fix")
                .created_via(format!("{CREATED_VIA_CI_FIX_PREFIX}cir_moot"))
                .build(),
            &checker,
        )
        .unwrap();
    let findings = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&parent.id)
                .description("Review findings")
                .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_findings"))
                .build(),
            &checker,
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked' WHERE id = ?1",
            rusqlite::params![findings.id],
        )
        .unwrap();
    db.mark_chore_pr_merged(&parent.id, pr).unwrap();
    assert_latest(
        &db,
        &in_review_rev.id,
        "status",
        "flip_in_review_revisions_to_done",
        "engine",
    );
    assert_latest(&db, &moot.id, "status", "archive_revision_task", "engine");
    let findings_rows: Vec<(String, String, String)> = {
        let conn = db.connect().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT property, basis, actor FROM task_status_audit
                 WHERE work_item_id = ?1 ORDER BY id",
            )
            .unwrap();
        stmt.query_map([&findings.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert!(
        findings_rows.iter().any(|(property, basis, actor)| {
            property.as_str() != "" && basis == "convert_revision_to_review_findings_followup" && actor == "engine"
        }),
        "expected convert_revision_to_review_findings_followup on findings ledger, got {findings_rows:?}"
    );

    let conflict_parent = create_test_chore(&db, product.clone(), "Conflict retry");
    bind_pr(&db, &conflict_parent.id, pr);
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: conflict_parent.id.clone(),
            pr_url: pr.into(),
            pr_number: 105,
            head_branch: "feat".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base".into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap()
        .unwrap();
    db.mark_conflict_resolution_failed(&attempt.id, "failed").unwrap();
    db.retry_conflict_resolution(&attempt.id).unwrap();
    assert_latest(
        &db,
        &conflict_parent.id,
        "blocked_reason",
        "retry_conflict_resolution",
        "engine",
    );

    let ladder = create_test_chore(&db, product.clone(), "Orphaned ladder");
    bind_pr(&db, &ladder.id, pr);
    db.mark_chore_blocked_merge_conflict(&ladder.id, pr).unwrap();
    let orphaned = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: ladder.id.clone(),
            pr_url: pr.into(),
            pr_number: 106,
            head_branch: "feat".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base2".into()),
            head_sha_before: Some("head2".into()),
        })
        .unwrap()
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET blocked_attempt_id = ?2 WHERE id = ?1",
            rusqlite::params![ladder.id, orphaned.id],
        )
        .unwrap();
    db.reconcile_orphaned_conflict_ladder_attempts().unwrap();
    assert_latest(
        &db,
        &ladder.id,
        "blocked_reason",
        "reconcile_orphaned_conflict_ladder_attempts",
        "engine",
    );

    let queue = create_test_chore(&db, product.clone(), "Queue rearm");
    bind_pr(&db, &queue.id, pr);
    let cir = db
        .insert_ci_remediation(ci_input(&product, &queue.id, pr))
        .unwrap()
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE ci_remediations SET status = 'abandoned', failure_reason = 'verified_requeue',
             finished_at = '1' WHERE id = ?1",
            rusqlite::params![cir.id],
        )
        .unwrap();
    let reloaded = db.get_ci_remediation(&cir.id).unwrap().unwrap();
    db.rearm_audited_queue_failure(&reloaded).unwrap();
    assert_latest(
        &db,
        &queue.id,
        "blocked_reason",
        "rearm_audited_queue_failure",
        "engine",
    );

    let prereq = create_test_chore(&db, product.clone(), "Unblock prereq");
    let dependent = create_test_chore(&db, product, "Unblock dependent");
    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: prereq.id.clone(),
        relation: None,
    })
    .unwrap();
    db.update_work_item(
        &prereq.id,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_latest(
        &db,
        &dependent.id,
        "blocked_reason",
        "maybe_engine_unblock_dependent",
        "engine",
    );
}

#[test]
fn operator_yes_and_prerequisite_approval_record_exact_bases() {
    let (db, product) = memory_db();
    let yes = create_test_chore(&db, product.clone(), "Yes restart");
    let yes_exec = start_waiting_human(&db, &yes.id);
    let question = OperatorQuestion {
        text: "Ship the change?".into(),
        explanation: "Need a decision.".into(),
        answer_type: OperatorAnswerType::YesNo,
    };
    crate::work::insert_question(&db.connect().unwrap(), &yes_exec.id, "proposal", &question).unwrap();
    db.record_worker_awaiting_operator_answer(&yes_exec.id, "needs a decision", &question)
        .unwrap();
    db.answer_operator_question(&yes.id, OperatorAnswer::YesNo { value: true })
        .unwrap();
    assert_latest(&db, &yes.id, "status", "restart_after_answer", "human");

    let gated = create_test_chore(&db, product, "Needs prereq");
    let gated_exec = start_waiting_human(&db, &gated.id);
    let prereq_q = OperatorQuestion {
        text: "Create the missing work?".into(),
        explanation: "This cannot proceed without it.".into(),
        answer_type: OperatorAnswerType::CreatePrerequisiteTask {
            name: "Prerequisite work".into(),
            brief: "Do the other thing first.".into(),
        },
    };
    crate::work::insert_question(&db.connect().unwrap(), &gated_exec.id, "proposal", &prereq_q).unwrap();
    db.record_worker_awaiting_operator_answer(&gated_exec.id, "needs a prereq", &prereq_q)
        .unwrap();
    db.answer_operator_question(&gated.id, OperatorAnswer::YesNo { value: true })
        .unwrap();
    assert_has(&db, &gated.id, "status", "approve_in_tx", "human");
}

#[test]
fn review_verdict_apply_records_cycle_root_advance() {
    let (db, product) = memory_db();
    let root = create_test_chore_manual(&db, product, "Review target");
    let pr = "https://github.com/example/repo/pull/42";
    set_pr_url(&db, &root.id, pr);
    let supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root.id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let classification = ReviewClassification::builder()
        .changed_files(vec!["src/lib.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["engine".to_owned()])
        .additions(1)
        .deletions(0)
        .build();
    let (batch, _) = db
        .create_review_batch(
            ReviewBatchCreateInput::builder()
                .cycle_root_id(&root.id)
                .base_sha("base-sha")
                .classification(classification)
                .phase(ReviewBatchPhase::PreMerge)
                .pr_number(42)
                .pr_url(pr)
                .target_sha("head-sha")
                .build(),
            &[ReviewBatchMemberCreateInput::builder()
                .attempt(1)
                .provider_effort("medium")
                .requested_driver("claude")
                .resolved_model("test-model")
                .role(ReviewBatchMemberRole::Supervisor)
                .status(ReviewBatchMemberStatus::Pending)
                .maybe_execution_id(Some(supervisor.id.clone()))
                .build()],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'supervising' WHERE id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
    let payload = format!(
        r#"{{"batch_id":"{}","verdict":{{"batch_id":"{}","pr_url":"{pr}","target_sha":"head-sha","phase":"pre_merge","summary":"Clean.","revision_warranted":false,"findings":[],"contradictions":[]}}}}"#,
        batch.id, batch.id
    );
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor.id,
            work_item_id: &root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &payload,
            idempotency_key: "verdict-ledger",
        })
        .unwrap()
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&supervisor.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = 'head-sha', ci_required_state = 'success', pr_mergeable_state = 'mergeable' WHERE id = ?1",
            [&root.id],
        )
        .unwrap();
    db.apply_review_verdict_proposal(
        &outcome.proposal.id,
        &FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head-sha"),
    )
    .unwrap();
    assert_latest(
        &db,
        &root.id,
        "status",
        "advance_cycle_root_to_in_review_in_tx",
        "engine",
    );
}

#[test]
fn reconcile_revision_execution_records_its_basis() {
    let (db, product) = memory_db();
    let pr = "https://github.com/spinyfin/mono/pull/107";
    let parent = create_test_chore(&db, product.clone(), "Reconcile parent");
    bind_pr(&db, &parent.id, pr);
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product,
            work_item_id: parent.id.clone(),
            pr_url: pr.into(),
            pr_number: 107,
            head_branch: "feat".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base".into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap()
        .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&parent.id)
                .description("Reconcile me")
                .created_via(format!("merge-conflict:{}", attempt.id))
                .build(),
            &checker,
        )
        .unwrap();
    db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id)
        .unwrap();
    db.mark_conflict_resolution_succeeded(&attempt.id, Some("head"))
        .unwrap();
    db.reconcile_product_executions(&parent.product_id).unwrap();
    assert_latest(&db, &revision.id, "status", "reconcile_revision_execution", "engine");
}
