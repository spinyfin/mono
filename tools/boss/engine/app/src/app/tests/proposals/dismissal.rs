use super::*;
use boss_protocol::{OperatorQuestionStatus, RequestExecutionInput, TaskStatus, WorkItemPatch};

async fn dismiss(state: &Arc<ServerState>, id: &str, question: Option<&str>, note: Option<&str>) -> FrontendEvent {
    call_with_peer(
        state,
        None,
        FrontendRequest::DismissOperatorQuestion {
            id: id.into(),
            question_id: question.map(str::to_owned),
            note: note.map(str::to_owned),
        },
    )
    .await
}

#[tokio::test]
async fn dismiss_question_has_no_answer_effects_and_requires_explicit_start() {
    for payload in [
        operator_question_payload(),
        prerequisite_question_payload(),
        multiple_choice::multiple_choice_payload(),
    ] {
        let (state, _dir, _, id) = parked_task_with_question(payload).await;
        let db = &state.work_db;
        let original = question_task(&state, &id);
        let question = original.operator_question.as_ref().unwrap();
        let tasks_before = db.list_tasks(&original.product_id, None, None, false).unwrap().len();
        let executions_before = db.list_executions(Some(&id)).unwrap().len();
        assert!(matches!(
            dismiss(&state, &id, Some(&question.id), Some("Neither option fits")).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        let task = question_task(&state, &id);
        assert_eq!(task.status, TaskStatus::Blocked);
        assert_eq!(task.blocked_reason.as_deref(), Some("question_dismissed"));
        assert_eq!(task.blocked_detail.as_deref(), Some("Neither option fits"));
        assert!(!task.autostart);
        assert!(task.operator_question.is_none());
        assert_eq!(task.description, original.description);
        assert_eq!(
            db.list_tasks(&original.product_id, None, None, false).unwrap().len(),
            tasks_before
        );
        assert!(db.gating_prereqs_for(&id).unwrap().is_empty());
        let history = db.list_operator_questions(&id).unwrap();
        assert_eq!(history[0].status, OperatorQuestionStatus::Dismissed);
        assert!(history[0].answer.is_none());
        assert!(history[0].answered_at.is_none());
        let disposition = history[0].dismissal.as_ref().unwrap();
        assert_eq!(disposition.by, "human");
        assert!(disposition.at.parse::<i64>().unwrap() > 0);
        assert_eq!(disposition.note.as_deref(), Some("Neither option fits"));
        // A retry retains the original disposition; a late answer cannot revive it.
        assert!(matches!(
            dismiss(&state, &question.id, None, Some("retry")).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        assert_eq!(db.list_operator_questions(&id).unwrap(), history);
        assert!(matches!(answer_question(&state, &question.id, true).await,
            FrontendEvent::OperatorQuestionError { error: boss_protocol::OperatorQuestionError::Conflict { state, answer: None } }
                if state == "dismissed"));
        db.reconcile_product_executions(&task.product_id).unwrap();
        db.reconcile_active_dispatch(|_| false).unwrap();
        assert!(db.mint_progression_execution(&id).unwrap().is_none());
        assert_eq!(db.list_executions(Some(&id)).unwrap().len(), executions_before);
        let audit_count: i64 = db.connect().unwrap().query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason' AND actor = 'human' AND basis LIKE 'operator_question_dismissed%'",
            [&id], |row| row.get(0)).unwrap();
        assert_eq!(audit_count, 1);
        let automatic = db
            .request_execution(RequestExecutionInput::builder().work_item_id(&id).build())
            .unwrap_err();
        assert!(automatic.to_string().contains("question dismissed"));
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(&id)
                    .explicit_start(true)
                    .build(),
            )
            .unwrap();
        assert_eq!(execution.status, boss_protocol::ExecutionStatus::Ready);
        assert!(question_task(&state, &id).blocked_reason.is_none());
        assert_eq!(db.list_operator_questions(&id).unwrap(), history);
    }
}

#[tokio::test]
async fn dismiss_question_preserves_dependency_gates_and_explicit_status_changes() {
    let (state, _dir, _, id) = parked_question_task().await;
    let db = &state.work_db;
    let task = question_task(&state, &id);
    let prerequisite =
        boss_engine_runtime::test_support::create_test_chore_manual(db, task.product_id, "Existing prerequisite");
    db.add_dependency(boss_protocol::AddDependencyInput {
        dependent: id.clone(),
        prerequisite: prerequisite.id.clone(),
        relation: Some("blocks".into()),
    })
    .unwrap();
    assert!(matches!(
        dismiss(&state, &id, None, None).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    assert_eq!(db.gating_prereqs_for(&id).unwrap(), vec![prerequisite.id.clone()]);
    let refused = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&id)
                .explicit_start(true)
                .build(),
        )
        .unwrap_err();
    assert!(refused.to_string().contains("gated by"));
    assert_eq!(
        question_task(&state, &id).blocked_reason.as_deref(),
        Some("question_dismissed")
    );
    db.update_work_item(&prerequisite.id, WorkItemPatch::builder().status("done").build())
        .unwrap();
    assert_eq!(
        question_task(&state, &id).blocked_reason.as_deref(),
        Some("question_dismissed")
    );
    assert_eq!(db.list_executions(Some(&id)).unwrap().len(), 1);
    db.update_work_item(&id, WorkItemPatch::builder().status("todo").build())
        .unwrap();
    assert!(question_task(&state, &id).blocked_reason.is_none());
}

#[tokio::test]
async fn dismiss_question_refuses_mismatched_question_and_closed_question() {
    let (state, _dir, _, id) = parked_question_task().await;
    assert!(matches!(
        dismiss(&state, &id, Some("oq_missing"), None).await,
        FrontendEvent::OperatorQuestionError {
            error: boss_protocol::OperatorQuestionError::NotFound
        }
    ));
    let question = question_task(&state, &id).operator_question.unwrap();
    assert!(matches!(
        answer_question(&state, &id, false).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    assert!(matches!(dismiss(&state, &question.id, None, None).await,
        FrontendEvent::OperatorQuestionError { error: boss_protocol::OperatorQuestionError::Conflict { state, .. } } if state == "answered"));
}

#[tokio::test]
async fn dismiss_question_upgrade_keeps_history_and_open_question_projection() {
    let (state, dir, _, id) = parked_question_task().await;
    let before = state.work_db.list_operator_questions(&id).unwrap();
    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "ALTER TABLE operator_questions DROP COLUMN dismissal_json;
         DELETE FROM schema_migrations WHERE id = '20261117_0210_operator_question_dismissal';",
        )
        .unwrap();
    let upgraded = boss_work_db::WorkDb::open(dir.path().join("state.db")).unwrap();
    assert_eq!(upgraded.list_operator_questions(&id).unwrap(), before);
    assert!(question_task(&state, &id).operator_question.is_some());
    upgraded
        .dismiss_operator_question_from(&id, None, None, "human", Some("upgrade test"))
        .unwrap();
    let reopened = boss_work_db::WorkDb::open(dir.path().join("state.db")).unwrap();
    assert_eq!(
        reopened.list_operator_questions(&id).unwrap()[0].status,
        OperatorQuestionStatus::Dismissed
    );
    assert!(question_task(&state, &id).operator_question.is_none());
}

#[tokio::test]
async fn dismiss_question_on_revision_survives_automatic_reconciliation() {
    let (state, _dir, _, id) = parked_task_with_question_kind(operator_question_payload(), true).await;
    let original = question_task(&state, &id);
    let executions = state.work_db.list_executions(Some(&id)).unwrap().len();
    assert!(matches!(
        dismiss(&state, &id, None, None).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    state
        .work_db
        .reconcile_product_executions(&original.product_id)
        .unwrap();
    state.work_db.reconcile_active_dispatch(|_| false).unwrap();
    assert_eq!(
        question_task(&state, &id).blocked_reason.as_deref(),
        Some("question_dismissed")
    );
    assert_eq!(state.work_db.list_executions(Some(&id)).unwrap().len(), executions);
    assert!(state.work_db.mint_progression_execution(&id).unwrap().is_none());
}
