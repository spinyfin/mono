//! `architectural_mismatch` on a merge-conflict attempt raises a question for
//! the repo owner instead of dead-ending the PR.

use super::*;
use boss_protocol::{
    ArchitecturalMismatchEvidence, CONFLICT_MISMATCH_CHOICE_FORCE_MERGE, CONFLICT_MISMATCH_CHOICE_REWRITE,
    MismatchBasis, OperatorAnswer, OperatorAnswerType, OperatorQuestionStatus, ProposalKind, RunDoneOutcome,
    RunDoneProposalPayload,
};

const PR_URL: &str = "https://github.com/spinyfin/mono/pull/3405";

struct Fixture {
    db: WorkDb,
    chore_id: String,
    crz_id: String,
    revision_id: String,
    execution_id: String,
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

fn blocked_fields(db: &WorkDb, task_id: &str) -> (Option<String>, Option<String>) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT blocked_reason, blocked_detail FROM tasks WHERE id = ?1",
            params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn open_pr() -> FakePrStateChecker {
    FakePrStateChecker::always(PrOpenState::Open)
}

/// An in-review PR whose conflict revision is running (the attempt is live).
fn fixture() -> Fixture {
    let db = WorkDb::open(temp_db_path("conflict-mismatch")).unwrap();
    let product_id = make_revision_product(&db, "conflict-mismatch");
    let chore_id = make_in_review_chore(&db, &product_id, PR_URL);
    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: PR_URL.to_owned(),
            pr_number: 3405,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap()
        .unwrap();
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(chore_id.clone())
                .description("Resolve merge conflict against main")
                .created_via(format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{}", crz.id))
                .effort_level(EffortLevel::Trivial)
                .build(),
            &open_pr(),
        )
        .unwrap();
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision.id)
        .unwrap();
    let execution_id = executions_for(&db, &revision.id)[0].0.clone();
    start_run(&db, &execution_id, "worker-1");
    assert_eq!(task_status(&db, &revision.id), "active");
    Fixture {
        db,
        chore_id,
        crz_id: crz.id,
        revision_id: revision.id,
        execution_id,
    }
}

fn evidence() -> ArchitecturalMismatchEvidence {
    ArchitecturalMismatchEvidence {
        basis: MismatchBasis::SubsystemDeleted,
        conflicting_commits: "all six commits of the PR".into(),
        main_changes: "the review-queue subsystem was deleted outright".into(),
        attempted_port: "mapped every touched symbol onto main and found no queue to extend".into(),
        contradiction: "Main deleted the review queue entirely, so the PR's new queue priority has nowhere to live."
            .into(),
        recommendation: "Rewrite the priority around the dispatcher.".into(),
    }
}

fn declare_blocked(f: &Fixture) {
    declare_blocked_keyed(f, "declare-blocked");
}

fn declare_blocked_keyed(f: &Fixture, idempotency_key: &str) {
    let payload = serde_json::to_string(&RunDoneProposalPayload {
        outcome: RunDoneOutcome::Blocked,
        summary: "Architectural mismatch".into(),
        question: None,
        queue_ci_evidence: None,
    })
    .unwrap();
    f.db.submit_worker_proposal(crate::SubmitWorkerProposalInput {
        execution_id: &f.execution_id,
        work_item_id: &f.revision_id,
        kind: ProposalKind::RunDone,
        payload_json: &payload,
        idempotency_key,
    })
    .unwrap()
    .unwrap();
}

fn latest_execution(f: &Fixture, work_item_id: &str) -> (String, String) {
    executions_for(&f.db, work_item_id)
        .into_iter()
        .next_back()
        .expect("execution")
}

fn open_linked_question_count(f: &Fixture) -> i64 {
    f.db.connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM operator_questions
             WHERE work_item_id = ?1 AND status = 'open' AND conflict_resolution_id IS NOT NULL",
            [&f.revision_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn mismatch_evidence_present(f: &Fixture) -> bool {
    f.db.connect()
        .unwrap()
        .query_row(
            "SELECT mismatch_evidence_json IS NOT NULL FROM conflict_resolutions WHERE id = ?1",
            [&f.crz_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// Run the whole worker side: record, declare blocked, park on the question.
fn raise_question(f: &Fixture) {
    f.db.record_conflict_architectural_mismatch(&f.crz_id, Some(&evidence()))
        .unwrap()
        .expect("live attempt");
    declare_blocked(f);
    let question =
        f.db.execution_operator_question(&f.execution_id)
            .unwrap()
            .expect("the engine asks on the worker's behalf");
    f.db.record_worker_awaiting_operator_answer(&f.execution_id, "worker declared blocked", &question)
        .unwrap()
        .expect("live execution parks the revision");
}

fn open_questions(f: &Fixture) -> Vec<boss_protocol::OperatorQuestionRecord> {
    f.db.list_operator_questions(&f.revision_id)
        .unwrap()
        .into_iter()
        .filter(|record| record.status == OperatorQuestionStatus::Open)
        .collect()
}

#[test]
fn a_refactor_only_conflict_is_rejected_and_raises_no_question() {
    let f = fixture();
    for (contradiction, attempted) in [
        (
            "The coordinator files moved into the dispatch crate and the helpers became private.",
            "tried a rebase",
        ),
        (
            "Every commit conflicts and this would be a large rewrite of the module.",
            "looked",
        ),
        (
            "Too many conflicts across the whole branch to resolve by hand now.",
            "looked",
        ),
    ] {
        let refactor_only = ArchitecturalMismatchEvidence {
            basis: MismatchBasis::ConceptRedesigned,
            contradiction: contradiction.into(),
            attempted_port: attempted.into(),
            ..evidence()
        };
        let err =
            f.db.record_conflict_architectural_mismatch(&f.crz_id, Some(&refactor_only))
                .unwrap_err();
        assert!(format!("{err:#}").contains("normal conflict work"), "{err:#}");
    }
    // The worker then declares blocked anyway: no question, the ordinary failure.
    declare_blocked(&f);
    assert!(f.db.execution_operator_question(&f.execution_id).unwrap().is_none());
    assert!(open_questions(&f).is_empty());
    let attempt = f.db.get_conflict_resolution(&f.crz_id).unwrap().unwrap();
    assert_eq!(attempt.status, "pending");
}

#[test]
fn missing_evidence_is_rejected() {
    let f = fixture();
    let err =
        f.db.record_conflict_architectural_mismatch(&f.crz_id, None)
            .unwrap_err();
    assert!(format!("{err:#}").contains("requires evidence"), "{err:#}");
    assert!(open_questions(&f).is_empty());
    assert_eq!(
        f.db.get_conflict_resolution(&f.crz_id).unwrap().unwrap().status,
        "pending"
    );
}

#[test]
fn a_mismatch_raises_a_question_and_leaves_no_dead_end() {
    let f = fixture();
    raise_question(&f);

    // The revision waits on the repo owner; it does not look failed.
    assert_eq!(task_status(&f.db, &f.revision_id), "blocked");
    assert_eq!(
        blocked_fields(&f.db, &f.revision_id).0.as_deref(),
        Some("awaiting_operator_answer")
    );
    let questions = open_questions(&f);
    assert_eq!(questions.len(), 1, "{questions:?}");
    let OperatorAnswerType::MultipleChoice { choices } = &questions[0].question.answer_type else {
        panic!("expected a multiple-choice question");
    };
    assert_eq!(
        choices,
        &[
            CONFLICT_MISMATCH_CHOICE_REWRITE.to_owned(),
            CONFLICT_MISMATCH_CHOICE_FORCE_MERGE.to_owned()
        ]
    );
    let explanation = &questions[0].question.explanation;
    for needle in [
        "all six commits of the PR",
        "review-queue subsystem was deleted",
        "mapped every touched symbol",
        "Rewrite the priority around the dispatcher.",
    ] {
        assert!(explanation.contains(needle), "{needle}\n{explanation}");
    }

    // The attempt is still live and the revision still its fix vehicle, so the
    // poller neither supersedes nor duplicates it.
    let attempt = f.db.get_conflict_resolution(&f.crz_id).unwrap().unwrap();
    assert_eq!(attempt.status, "pending");
    assert_eq!(attempt.failure_reason, None);
    assert!(f.db.is_conflict_revision_running_or_queued(&f.revision_id).unwrap());
    assert!(f.db.has_live_conflict_revision_for_pr(PR_URL).unwrap());

    // A second declaration with the same idempotency key is a replay and
    // does not ask again.
    declare_blocked(&f);
    assert!(f.db.execution_operator_question(&f.execution_id).unwrap().is_some());
    assert!(
        mismatch_evidence_present(&f),
        "evidence stays until the question is answered"
    );
}

#[test]
fn a_second_distinct_blocked_declaration_re_raises_the_withdrawn_question() {
    let f = fixture();
    raise_question(&f);
    assert_eq!(open_linked_question_count(&f), 1);
    assert!(mismatch_evidence_present(&f));

    declare_blocked_keyed(&f, "declare-blocked-again");
    assert_eq!(open_questions(&f).len(), 1, "{:?}", open_questions(&f));
    assert_eq!(open_linked_question_count(&f), 1);
    assert!(f.db.is_conflict_revision_running_or_queued(&f.revision_id).unwrap());
    assert!(mismatch_evidence_present(&f));

    let outcome =
        f.db.answer_operator_question_with(
            &f.revision_id,
            OperatorAnswer::MultipleChoice {
                value: CONFLICT_MISMATCH_CHOICE_FORCE_MERGE.into(),
            },
            &open_pr(),
        )
        .unwrap();
    assert_eq!(outcome.dispatch_work_item_id.as_deref(), Some(f.revision_id.as_str()));
    assert_eq!(open_questions(&f).len(), 0);
    assert!(!mismatch_evidence_present(&f));
}

#[test]
fn a_second_distinct_blocked_declaration_still_allows_rewrite() {
    let f = fixture();
    raise_question(&f);
    declare_blocked_keyed(&f, "declare-blocked-again");
    let outcome =
        f.db.answer_operator_question_with(
            &f.revision_id,
            OperatorAnswer::MultipleChoice {
                value: CONFLICT_MISMATCH_CHOICE_REWRITE.into(),
            },
            &open_pr(),
        )
        .unwrap();
    let port_id = outcome.dispatch_work_item_id.expect("the port revision is launched");
    assert_ne!(port_id, f.revision_id);
    assert_eq!(task_status(&f.db, &f.revision_id), "archived");
    assert_eq!(executions_for(&f.db, &port_id).last().unwrap().1, "ready");
}

#[test]
fn force_a_merge_redispatches_the_conflict_revision_and_closes_the_exit() {
    let f = fixture();
    raise_question(&f);
    let outcome =
        f.db.answer_operator_question_with(
            &f.revision_id,
            OperatorAnswer::MultipleChoice {
                value: CONFLICT_MISMATCH_CHOICE_FORCE_MERGE.into(),
            },
            &open_pr(),
        )
        .unwrap();
    assert!(outcome.minted_execution);
    assert_eq!(outcome.dispatch_work_item_id.as_deref(), Some(f.revision_id.as_str()));
    assert_eq!(task_status(&f.db, &f.revision_id), "todo");
    let executions = executions_for(&f.db, &f.revision_id);
    assert_eq!(executions.len(), 2, "{executions:?}");
    assert_eq!(executions[1].1, "ready");
    assert!(
        f.db.execution_has_operator_forced_admission(&executions[1].0).unwrap(),
        "Force keeps operator-forced admission until the work is admitted"
    );

    let WorkItem::Task(task) = f.db.get_work_item(&f.revision_id).unwrap() else {
        panic!("expected a task");
    };
    assert!(
        task.description.contains("Resolve the conflicts in place"),
        "{}",
        task.description
    );
    assert!(task.description.contains("no longer an allowed exit"));
    crate::prose_vocabulary::assert_no_forbidden_prose("force-merge answer", &task.description);

    let attempt = f.db.get_conflict_resolution(&f.crz_id).unwrap().unwrap();
    assert!(attempt.mismatch_exit_barred);
    assert_eq!(attempt.status, "pending");
    let err =
        f.db.record_conflict_architectural_mismatch(&f.crz_id, Some(&evidence()))
            .unwrap_err();
    assert!(format!("{err:#}").contains("no longer an allowed exit"), "{err:#}");
}

/// Answering a conflict revision gated behind another writer succeeds and
/// leaves a waiting execution for the dependency cascade to promote.
#[test]
fn answering_a_gated_conflict_revision_parks_it_behind_its_gate_instead_of_failing() {
    let f = fixture();
    raise_question(&f);
    let product_id = f.db.get_work_item(&f.chore_id).unwrap().product_id().to_owned();
    let writer = insert_revision_row(&f.db, &product_id, &f.chore_id);
    {
        let conn = f.db.connect().unwrap();
        conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&writer])
            .unwrap();
        deps::insert_edge(&conn, &f.revision_id, &writer, RELATION_BLOCKS, &now_string()).unwrap();
    }
    let outcome =
        f.db.answer_operator_question_with(
            &f.revision_id,
            OperatorAnswer::MultipleChoice {
                value: CONFLICT_MISMATCH_CHOICE_FORCE_MERGE.into(),
            },
            &open_pr(),
        )
        .expect("a gated item can still be answered");
    assert!(!outcome.minted_execution, "the gate decides when it starts");
    let executions = executions_for(&f.db, &f.revision_id);
    assert_eq!(executions.last().unwrap().1, "waiting_dependency", "{executions:?}");
    assert!(
        f.db.execution_has_operator_forced_admission(&executions.last().unwrap().0)
            .unwrap(),
        "a gated Force still records operator-forced admission for the cascade"
    );
    assert!(open_questions(&f).is_empty());
    assert!(
        f.db.get_conflict_resolution(&f.crz_id)
            .unwrap()
            .unwrap()
            .mismatch_exit_barred
    );
}

#[test]
fn rewrite_replaces_the_fix_with_a_port_sized_revision_ahead_of_review_revisions() {
    let f = fixture();
    // A review-finding revision queued on the same PR behind the conflict fix.
    let review =
        f.db.create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(f.chore_id.clone())
                .description("Address review findings")
                .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}{}", next_id("exec")))
                .build(),
            &open_pr(),
        )
        .unwrap();
    raise_question(&f);

    let outcome =
        f.db.answer_operator_question_with(
            &f.revision_id,
            OperatorAnswer::MultipleChoice {
                value: CONFLICT_MISMATCH_CHOICE_REWRITE.into(),
            },
            &open_pr(),
        )
        .unwrap();
    let port_id = outcome
        .dispatch_work_item_id
        .clone()
        .expect("the port revision is launched");
    assert_ne!(port_id, f.revision_id);
    assert!(outcome.touched_work_item_ids.contains(&port_id));

    // The parked fix is retired; nothing is left waiting on the question.
    assert_eq!(task_status(&f.db, &f.revision_id), "archived");
    assert!(open_questions(&f).is_empty());

    // The port is a revision on the same PR, briefed and sized as a port.
    let WorkItem::Task(port) = f.db.get_work_item(&port_id).unwrap() else {
        panic!("expected a task");
    };
    assert_eq!(port.kind, TaskKind::Revision);
    assert_eq!(port.parent_task_id.as_deref(), Some(f.chore_id.as_str()));
    assert_eq!(port.effort_level, Some(EffortLevel::Large));
    assert_ne!(port.effort_level, Some(EffortLevel::Trivial));
    for needle in [
        "Port PR #3405 onto the current `main` branch",
        "every earlier review fix",
        "Do not stop on the ground of architectural mismatch",
        "effort-escalation marker",
        "review-queue subsystem was deleted",
    ] {
        assert!(port.description.contains(needle), "{needle}\n{}", port.description);
    }
    crate::prose_vocabulary::assert_no_forbidden_prose("port brief", &port.description);

    // The attempt follows the port and can no longer exit on a mismatch.
    let attempt = f.db.get_conflict_resolution(&f.crz_id).unwrap().unwrap();
    assert_eq!(attempt.revision_task_id.as_deref(), Some(port_id.as_str()));
    assert!(attempt.mismatch_exit_barred);
    assert_eq!(attempt.status, "pending");
    let port_exec = latest_execution(&f, &port_id);
    assert_eq!(port_exec.1, "ready");
    assert!(
        f.db.execution_has_operator_forced_admission(&port_exec.0).unwrap(),
        "Rewrite keeps operator-forced admission on the port execution"
    );

    // Review revisions queued on the PR run after the port.
    let gates = deps::gating_prereqs_for(&f.db.connect().unwrap(), &review.id).unwrap();
    assert!(
        gates.contains(&port_id),
        "the review revision must wait for the port: {gates:?}"
    );
    assert_eq!(
        executions_for(&f.db, &review.id).last().unwrap().1,
        "waiting_dependency"
    );
}

#[test]
fn an_answer_that_does_not_belong_to_the_question_is_refused() {
    let f = fixture();
    raise_question(&f);
    let err =
        f.db.answer_operator_question_with(
            &f.revision_id,
            OperatorAnswer::MultipleChoice {
                value: "Abandon the PR".into(),
            },
            &open_pr(),
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("not one of the offered choices"), "{err:#}");
    assert_eq!(open_questions(&f).len(), 1, "the question stays open");
}
