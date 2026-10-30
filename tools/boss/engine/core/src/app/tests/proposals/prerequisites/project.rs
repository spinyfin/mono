use super::*;
use boss_protocol::{CreateProjectInput, CreateTaskInput, ExecutionStatus, TaskKind, WorkItemPatch};

fn finish(state: &ServerState, id: &str) {
    state
        .work_db
        .update_work_item(
            id,
            WorkItemPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
}

#[tokio::test]
async fn reused_project_prerequisite_moves_its_dependency_closure() {
    let (state, _dir, _, task_id) = parked_task_with_question(prerequisite_question_payload()).await;
    let db = &state.work_db;
    let product_id = question_task(&state, &task_id).product_id;
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(&product_id)
                .name("Dependency order")
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
    let create = |name: &str| {
        db.create_task(
            CreateTaskInput::builder()
                .product_id(&product_id)
                .project_id(&project.id)
                .name(name)
                .build(),
        )
        .unwrap()
    };
    let upstream = create("Upstream repair");
    let candidate = create(PREREQUISITE_NAME);
    db.add_dependency(boss_protocol::AddDependencyInput {
        dependent: candidate.id.clone(),
        prerequisite: upstream.id.clone(),
        relation: None,
    })
    .unwrap();
    assert!(question_task(&state, &task_id).ordinal < upstream.ordinal);
    assert!(upstream.ordinal < candidate.ordinal);
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    assert_eq!(prerequisites_of(&state, &task_id), vec![candidate.id.clone()]);
    db.reconcile_product_executions(&product_id).unwrap();
    assert_execution_status(&state, &upstream.id, ExecutionStatus::Ready);
    assert_execution_status(&state, &candidate.id, ExecutionStatus::WaitingDependency);
    assert_execution_status(&state, &task_id, ExecutionStatus::WaitingDependency);
    finish(&state, &upstream.id);
    db.reconcile_product_executions(&product_id).unwrap();
    assert_execution_status(&state, &candidate.id, ExecutionStatus::Ready);
    finish(&state, &candidate.id);
    db.reconcile_product_executions(&product_id).unwrap();
    assert_execution_status(&state, &task_id, ExecutionStatus::Ready);
}

#[tokio::test]
async fn project_revision_prerequisite_precedes_downstream_work() {
    for attention_route in [false, true] {
        let (state, _dir, _, revision_id) =
            parked_task_with_question_route(prerequisite_question_payload(), true, Some(attention_route)).await;
        let db = &state.work_db;
        let revision = question_task(&state, &revision_id);
        assert!(revision.ordinal.is_none());
        if attention_route {
            assert_eq!(revision.created_via, "attention");
        }
        let root_id = revision.parent_task_id.as_ref().unwrap();
        let downstream = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(&revision.product_id)
                    .project_id(revision.project_id.as_ref().unwrap())
                    .name("Downstream work")
                    .build(),
            )
            .unwrap();
        db.add_dependency(boss_protocol::AddDependencyInput {
            dependent: downstream.id.clone(),
            prerequisite: root_id.clone(),
            relation: None,
        })
        .unwrap();
        assert!(matches!(
            answer_question(&state, &revision_id, true).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        let prerequisite_id = prerequisites_of(&state, &revision_id).pop().unwrap();
        assert!(question_task(&state, &prerequisite_id).ordinal < question_task(&state, root_id).ordinal);
        db.reconcile_product_executions(&revision.product_id).unwrap();
        assert_execution_status(&state, &prerequisite_id, ExecutionStatus::Ready);
        assert_execution_status(&state, &downstream.id, ExecutionStatus::WaitingDependency);
        finish(&state, &prerequisite_id);
        db.reconcile_product_executions(&revision.product_id).unwrap();
        assert_execution_status(&state, &revision_id, ExecutionStatus::Ready);
        assert_execution_status(&state, &downstream.id, ExecutionStatus::WaitingDependency);
    }
}

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
