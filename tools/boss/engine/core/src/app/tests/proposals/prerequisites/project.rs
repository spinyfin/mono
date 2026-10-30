use super::*;
use boss_protocol::{CreateProjectInput, CreateTaskInput, ExecutionStatus, TaskKind, WorkItemPatch};

fn assert_execution_status(state: &ServerState, id: &str, expected: ExecutionStatus) {
    let executions = state.work_db.list_executions(Some(id)).unwrap();
    assert_eq!(executions.last().unwrap().status, expected, "{id}: {executions:?}");
}

#[tokio::test]
async fn project_prerequisite_creation_and_reuse_dispatch_before_the_dependent() {
    for candidate_project in [None, Some(true), Some(false)] {
        let (state, _dir, _, task_id) = parked_task_with_question(prerequisite_question_payload()).await;
        let db = &state.work_db;
        let product_id = question_task(&state, &task_id).product_id;
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(&product_id)
                    .name("Source project")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &task_id,
            WorkItemPatch {
                project_id: Some(project.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let original = question_task(&state, &task_id);
        assert_eq!(original.kind, TaskKind::ProjectTask);
        let candidate = candidate_project.map(|same_project| {
            let project_id = if same_project {
                project.id.clone()
            } else {
                db.create_project(
                    CreateProjectInput::builder()
                        .product_id(&product_id)
                        .name("Other project")
                        .no_design_task(true)
                        .build(),
                )
                .unwrap()
                .id
            };
            db.create_task(
                CreateTaskInput::builder()
                    .product_id(&product_id)
                    .project_id(project_id)
                    .name(PREREQUISITE_NAME)
                    .autostart(false)
                    .build(),
            )
            .unwrap()
        });
        let before = db.list_tasks(&product_id, None, None, false).unwrap().len();
        let outcome = db
            .answer_operator_question(&task_id, boss_protocol::OperatorAnswer::YesNo { value: true })
            .unwrap();
        let prerequisite_id = outcome.prerequisite_work_item_id.unwrap();
        let reused = candidate_project == Some(true);
        assert_eq!(
            db.list_tasks(&product_id, None, None, false).unwrap().len(),
            before + usize::from(!reused)
        );
        if let Some(candidate) = candidate {
            assert_eq!(prerequisite_id == candidate.id, reused);
        }
        assert_eq!(
            question_task(&state, &task_id).description.contains("An equivalent"),
            reused
        );
        let prerequisite = question_task(&state, &prerequisite_id);
        assert_eq!(prerequisite.project_id.as_deref(), Some(project.id.as_str()));
        assert!(prerequisite.ordinal < question_task(&state, &task_id).ordinal);
        db.reconcile_product_executions(&product_id).unwrap();
        assert_execution_status(&state, &prerequisite_id, ExecutionStatus::Ready);
        assert_execution_status(&state, &task_id, ExecutionStatus::WaitingDependency);
        db.update_work_item(
            &prerequisite_id,
            WorkItemPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.reconcile_product_executions(&product_id).unwrap();
        assert_execution_status(&state, &task_id, ExecutionStatus::Ready);
    }
}
