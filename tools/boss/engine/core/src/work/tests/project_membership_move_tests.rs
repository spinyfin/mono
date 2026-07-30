//! `WorkItemPatch::project_id`: moving a leaf work item between project
//! membership states (chore ⇄ project_task, project A ⇄ project B),
//! keeping `kind`/`ordinal` coherent, and refusing kinds that carry
//! their own membership semantics.

use super::*;

fn create_test_project(db: &WorkDb, product_id: impl Into<String>, name: &str) -> Project {
    db.create_project(CreateProjectInput {
        product_id: product_id.into(),
        name: name.to_owned(),
        description: None,
        goal: None,
        autostart: true,
        no_design_task: true,
    })
    .unwrap()
}

fn create_test_project_task(
    db: &WorkDb,
    product_id: impl Into<String>,
    project_id: impl Into<String>,
    name: &str,
) -> Task {
    db.create_task(
        CreateTaskInput::builder()
            .product_id(product_id)
            .project_id(project_id)
            .name(name)
            .build(),
    )
    .unwrap()
}

#[test]
fn chore_moves_into_project_becomes_project_task_with_ordinal() {
    let path = temp_db_path("move-chore-into-project");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = create_test_project(&db, product.id.clone(), "Target project");
    let chore = create_test_chore_manual(&db, product.id.clone(), "A chore");
    let short_id = chore.short_id;

    let updated = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                project_id: Some(project.id.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Task(task) = updated else {
        panic!("expected a project_task after moving into a project, got {updated:?}");
    };
    assert_eq!(task.id, chore.id, "move must preserve the primary id");
    assert_eq!(task.short_id, short_id, "move must preserve the short id");
    assert_eq!(task.kind, TaskKind::ProjectTask);
    assert_eq!(task.project_id.as_deref(), Some(project.id.as_str()));
    assert_eq!(task.ordinal, Some(1));

    let listed = db.list_tasks(&product.id, Some(&project.id), None, false).unwrap();
    assert!(
        listed.iter().any(|t| t.id == chore.id),
        "must appear in boss task list --project"
    );
    let chores = db.list_chores(&product.id, None, false).unwrap();
    assert!(
        !chores.iter().any(|c| c.id == chore.id),
        "must no longer appear in boss chore list"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn project_task_moves_to_no_project_becomes_chore_ordinal_cleared() {
    let path = temp_db_path("move-project-task-to-no-project");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = create_test_project(&db, product.id.clone(), "Source project");
    let project_task = create_test_project_task(&db, product.id.clone(), project.id.clone(), "A project task");
    let short_id = project_task.short_id;
    assert_eq!(project_task.ordinal, Some(1));

    let updated = db
        .update_work_item(
            &project_task.id,
            WorkItemPatch {
                project_id: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Chore(task) = updated else {
        panic!("expected a chore after moving out of its project, got {updated:?}");
    };
    assert_eq!(task.id, project_task.id, "move must preserve the primary id");
    assert_eq!(task.short_id, short_id, "move must preserve the short id");
    assert_eq!(task.kind, TaskKind::Chore);
    assert_eq!(task.project_id, None);
    assert_eq!(task.ordinal, None);

    let chores = db.list_chores(&product.id, None, false).unwrap();
    assert!(
        chores.iter().any(|c| c.id == project_task.id),
        "must appear in boss chore list"
    );
    let listed = db.list_tasks(&product.id, Some(&project.id), None, false).unwrap();
    assert!(
        !listed.iter().any(|t| t.id == project_task.id),
        "must no longer appear in the source project"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn project_task_moves_between_projects_reassigns_ordinal() {
    let path = temp_db_path("move-project-task-between-projects");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project_a = create_test_project(&db, product.id.clone(), "Project A");
    let project_b = create_test_project(&db, product.id.clone(), "Project B");
    // Give project B an existing task so the next-ordinal allocator has
    // something to be greater than — otherwise a reassignment to
    // ordinal 1 in an empty project B would be indistinguishable from a
    // bug that just left the project-A ordinal untouched.
    create_test_project_task(&db, product.id.clone(), project_b.id.clone(), "Existing B task");
    let project_task = create_test_project_task(&db, product.id.clone(), project_a.id.clone(), "Movable task");
    let short_id = project_task.short_id;

    let updated = db
        .update_work_item(
            &project_task.id,
            WorkItemPatch {
                project_id: Some(project_b.id.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Task(task) = updated else {
        panic!("expected a project_task after moving between projects, got {updated:?}");
    };
    assert_eq!(task.short_id, short_id);
    assert_eq!(task.kind, TaskKind::ProjectTask);
    assert_eq!(task.project_id.as_deref(), Some(project_b.id.as_str()));
    assert_eq!(
        task.ordinal,
        Some(2),
        "must be assigned a fresh ordinal scoped to project B"
    );

    let in_a = db.list_tasks(&product.id, Some(&project_a.id), None, false).unwrap();
    assert!(
        !in_a.iter().any(|t| t.id == project_task.id),
        "must be gone from project A"
    );
    let in_b = db.list_tasks(&product.id, Some(&project_b.id), None, false).unwrap();
    assert!(in_b.iter().any(|t| t.id == project_task.id), "must appear in project B");

    let _ = std::fs::remove_file(path);
}

/// Regression guard for the "forbidden shortcut" the design explicitly
/// calls out: a move must never go through delete-and-recreate, so
/// execution history attached to the row must survive untouched.
#[test]
fn move_preserves_execution_history() {
    let path = temp_db_path("move-preserves-execution-history");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = create_test_project(&db, product.id.clone(), "Target project");
    let chore = create_test_chore_manual(&db, product.id.clone(), "A chore with history");
    let execution = create_ready_chore_execution(&db, chore.id.clone());

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            project_id: Some(project.id.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let refetched = db.get_execution(&execution.id).unwrap();
    assert_eq!(
        refetched.work_item_id, chore.id,
        "execution must still be bound to the same work item"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn moving_into_a_project_from_another_product_is_rejected() {
    let path = temp_db_path("move-cross-product-rejected");
    let db = WorkDb::open(path.clone()).unwrap();
    let product_a = create_test_product_named(&db, "Product A");
    let product_b = create_test_product_named(&db, "Product B");
    let other_project = create_test_project(&db, product_b.id.clone(), "Other product's project");
    let chore = create_test_chore_manual(&db, product_a.id.clone(), "A chore");

    let err = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                project_id: Some(other_project.id.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("does not belong to product"),
        "expected a cross-product membership error, got: {err:#}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn moving_to_current_project_is_a_noop() {
    let path = temp_db_path("move-to-current-project-noop");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = create_test_project(&db, product.id.clone(), "Same project");
    let project_task = create_test_project_task(&db, product.id.clone(), project.id.clone(), "Task");

    let updated = db
        .update_work_item(
            &project_task.id,
            WorkItemPatch {
                project_id: Some(project.id.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Task(task) = updated else {
        panic!("expected still a project_task, got {updated:?}");
    };
    assert_eq!(task.ordinal, Some(1), "a no-op move must not reassign the ordinal");

    let _ = std::fs::remove_file(path);
}

#[test]
fn design_task_kind_is_refused_for_project_move() {
    let path = temp_db_path("move-refuses-design-kind");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Project with a design task".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        })
        .unwrap();
    let seed_tasks = db.list_tasks(&product.id, Some(&project.id), None, false).unwrap();
    let design_task = seed_tasks
        .iter()
        .find(|t| t.kind == TaskKind::Design)
        .expect("create_project with no_design_task=false must seed a design task");

    let other_project = create_test_project(&db, product.id.clone(), "Some other project");
    let err = db
        .update_work_item(
            &design_task.id,
            WorkItemPatch {
                project_id: Some(other_project.id.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("design"),
        "error must name the refused kind, got: {message}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn unset_project_on_already_project_less_chore_is_a_noop() {
    let path = temp_db_path("unset-project-noop");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Already a chore");

    let updated = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                project_id: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Chore(task) = updated else {
        panic!("expected still a chore, got {updated:?}");
    };
    assert_eq!(task.project_id, None);
    assert_eq!(task.ordinal, None);

    let _ = std::fs::remove_file(path);
}
