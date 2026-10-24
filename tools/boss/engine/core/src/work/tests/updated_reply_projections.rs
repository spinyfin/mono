use super::*;
use boss_protocol::{OperatorAnswer, OperatorAnswerType, OperatorQuestion, ReviewSection};

fn ready_task(db: &WorkDb) -> Task {
    let product = create_test_product(db);
    let task = create_test_chore(db, product.id, "Projected reply");
    make_ready(db, &task.id);
    task
}

fn make_ready(db: &WorkDb, id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', blocked_reason = NULL,
         pr_url = 'https://github.com/org/repo/pull/1', ci_required_state = 'success',
         pr_mergeable_state = 'mergeable' WHERE id = ?1",
            [id],
        )
        .unwrap();
}

fn assert_ready(task: &Task) {
    assert_eq!(task.review_section, Some(ReviewSection::Ready));
    assert!(task.ready_for_review);
}

fn task_from_item(item: WorkItem) -> Task {
    match item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected task, got {other:?}"),
    }
}

#[test]
fn set_task_doc_reply_projects_ready_section() {
    let db = WorkDb::open(temp_db_path("doc-reply-projection")).unwrap();
    let task = ready_task(&db);
    let reply = db
        .set_task_doc(SetTaskDocPointerInput {
            task_id: task.id,
            doc_path: Some("docs/review.md".into()),
            doc_branch: None,
            doc_repo_remote_url: None,
            unset: false,
        })
        .unwrap();
    assert_ready(&reply);
}

#[test]
fn external_ref_reply_projects_ready_section() {
    let db = WorkDb::open(temp_db_path("external-ref-reply-projection")).unwrap();
    let task = ready_task(&db);
    assert_ready(&task_from_item(db.get_task_with_external_ref(&task.id).unwrap()));
}

#[test]
fn operator_answer_and_repeat_project_current_section() {
    let db = WorkDb::open(temp_db_path("answer-reply-projection")).unwrap();
    let task = ready_task(&db);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&task.id])
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&task.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    super::super::pr_flow::insert_question(
        &db.connect().unwrap(),
        &execution.id,
        "proposal",
        &OperatorQuestion {
            text: "Continue?".into(),
            answer_type: OperatorAnswerType::YesNo,
            explanation: "Test question".into(),
        },
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'awaiting_operator_answer' WHERE id = ?1",
            [&task.id],
        )
        .unwrap();
    let revision = insert_revision_row(&db, &task.product_id, &task.id);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&revision])
        .unwrap();
    let answer = OperatorAnswer::YesNo { value: false };
    let first = task_from_item(db.answer_operator_question(&task.id, answer.clone()).unwrap().item);
    // First answers require a blocked task and cannot produce Ready: No
    // keeps it in Backlog, while Yes resumes it in Doing. Pin another
    // derived field so removing the first-answer projection still fails.
    assert_eq!(first.review_section, None);
    assert!(first.has_in_progress_revision);
    assert!(!first.ready_for_review);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&revision])
        .unwrap();
    make_ready(&db, &task.id);
    let repeat = task_from_item(db.answer_operator_question(&task.id, answer).unwrap().item);
    assert_ready(&repeat);
}
