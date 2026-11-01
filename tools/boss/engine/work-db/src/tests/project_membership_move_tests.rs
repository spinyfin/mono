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
        design_reasoning_effort_xhigh: false,
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
            design_reasoning_effort_xhigh: false,
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

// ── revision project-membership cascade ─────────────────────────────────
//
// A revision's project membership is derived from its parent at mint time
// (`insert_revision_in_tx`) and it has no independent identity to
// reassign — `update_task` refuses a direct `--set-project` on a
// `revision` row (see `direct_set_project_on_a_revision_is_still_refused`
// below). Moving the *parent's* project must therefore cascade onto every
// revision already minted against it, or the refusal makes the resulting
// divergence permanently unfixable through the CLI.

fn task_project_id(db: &WorkDb, task_id: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT project_id FROM tasks WHERE id = ?1",
            rusqlite::params![task_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
}

/// Assert the debug/consistency invariant this cascade exists to uphold:
/// no revision's `project_id` may differ from its parent's. Walks
/// `parent_task_id` one hop at a time so it also catches a revision whose
/// immediate parent is itself a revision.
fn assert_no_revision_project_divergence(db: &WorkDb) {
    let conn = db.connect().unwrap();
    let mut stmt = conn
        .prepare("SELECT id, project_id, parent_task_id FROM tasks WHERE kind = 'revision' AND deleted_at IS NULL")
        .unwrap();
    let revisions: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (rev_id, rev_project_id, parent_id) in revisions {
        let parent_id = parent_id.expect("a revision row must always carry a parent_task_id");
        let parent_project_id: Option<String> = conn
            .query_row(
                "SELECT project_id FROM tasks WHERE id = ?1",
                rusqlite::params![parent_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            rev_project_id, parent_project_id,
            "revision {rev_id} project_id must match its parent {parent_id}'s"
        );
    }
}

#[test]
fn moving_a_chore_with_a_revision_into_a_project_cascades_to_the_revision() {
    let path = temp_db_path("move-chore-with-revision-into-project");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = create_test_project(&db, product.id.clone(), "Target project");
    let chore = create_test_chore_manual(&db, product.id.clone(), "A chore with a revision");
    let revision = insert_revision_row(&db, &product.id, &chore.id);
    assert_eq!(task_project_id(&db, &revision), None, "revision starts project-less");

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            project_id: Some(project.id.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert_eq!(
        task_project_id(&db, &revision),
        Some(project.id.clone()),
        "moving the parent into a project must cascade to its existing revision"
    );
    assert_no_revision_project_divergence(&db);

    let _ = std::fs::remove_file(path);
}

#[test]
fn moving_a_project_task_with_a_revision_out_of_its_project_clears_the_revision() {
    let path = temp_db_path("unset-project-task-with-revision");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = create_test_project(&db, product.id.clone(), "Source project");
    let project_task = create_test_project_task(&db, product.id.clone(), project.id.clone(), "A project task");
    let revision = insert_revision_row(&db, &product.id, &project_task.id);
    // Simulate the revision having inherited the parent's project at mint
    // time, as `insert_revision_in_tx` does in production.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET project_id = ?2 WHERE id = ?1",
            rusqlite::params![revision, project.id],
        )
        .unwrap();
    assert_eq!(task_project_id(&db, &revision), Some(project.id.clone()));

    db.update_work_item(
        &project_task.id,
        WorkItemPatch {
            project_id: Some(String::new()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert_eq!(
        task_project_id(&db, &revision),
        None,
        "moving the parent out of its project must clear the revision's project too, not leave it stale"
    );
    assert_no_revision_project_divergence(&db);

    let _ = std::fs::remove_file(path);
}

#[test]
fn cascade_reaches_a_multi_level_revision_chain() {
    let path = temp_db_path("cascade-multi-level-revision-chain");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = create_test_project(&db, product.id.clone(), "Target project");
    let chore = create_test_chore_manual(&db, product.id.clone(), "Root chore");
    let revision_a = insert_revision_row(&db, &product.id, &chore.id);
    // A revision of a revision — legacy nesting that `collect_chain_revision_ids`
    // still walks (see its docs).
    let revision_b = insert_revision_row(&db, &product.id, &revision_a);

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            project_id: Some(project.id.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert_eq!(task_project_id(&db, &revision_a), Some(project.id.clone()));
    assert_eq!(
        task_project_id(&db, &revision_b),
        Some(project.id.clone()),
        "cascade must reach a revision nested two levels below the moved parent"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn moving_a_parent_updates_an_independently_deleted_revision_before_restore() {
    let path = temp_db_path("move-parent-with-deleted-revision");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let source_project = create_test_project(&db, product.id.clone(), "Source project");
    let target_project = create_test_project(&db, product.id.clone(), "Target project");
    let parent = create_test_project_task(&db, product.id.clone(), source_project.id.clone(), "Parent task");
    let revision = insert_revision_row(&db, &product.id, &parent.id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET project_id = ?2, deleted_at = 'independently-deleted' WHERE id = ?1",
            rusqlite::params![revision, source_project.id],
        )
        .unwrap();

    db.update_work_item(
        &parent.id,
        WorkItemPatch {
            project_id: Some(target_project.id.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let restored = db.restore_work_item(&revision).unwrap();
    let WorkItem::Task(restored) = restored else {
        panic!("expected restored revision task");
    };
    assert_eq!(restored.deleted_at, None, "revision must be restored independently");
    assert_eq!(
        task_project_id(&db, &revision),
        Some(target_project.id),
        "restoring an independently deleted revision must retain the parent's moved project"
    );
    assert_no_revision_project_divergence(&db);

    let _ = std::fs::remove_file(path);
}

#[test]
fn reapplying_the_current_project_still_repairs_a_drifted_revision() {
    // Regression guard for the one divergent row the design doc describes:
    // a revision minted before this cascade existed can be stuck with a
    // stale (or NULL) project_id even though its parent already carries the
    // right one. Re-applying `--set-project` with the project the parent
    // already has must still cascade and repair it, not short-circuit as a
    // no-op.
    let path = temp_db_path("noop-reapply-still-repairs-revision");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = create_test_project(&db, product.id.clone(), "Project");
    let project_task = create_test_project_task(&db, product.id.clone(), project.id.clone(), "Task");
    let revision = insert_revision_row(&db, &product.id, &project_task.id);
    assert_eq!(
        task_project_id(&db, &revision),
        None,
        "sanity: the revision starts divergent from its parent"
    );

    db.update_work_item(
        &project_task.id,
        WorkItemPatch {
            project_id: Some(project.id.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert_eq!(
        task_project_id(&db, &revision),
        Some(project.id.clone()),
        "a no-op re-application of the parent's current project must still cascade and repair the revision"
    );
    assert_no_revision_project_divergence(&db);

    let _ = std::fs::remove_file(path);
}

#[test]
fn direct_set_project_on_a_revision_is_still_refused() {
    let path = temp_db_path("direct-set-project-on-revision-refused");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = create_test_project(&db, product.id.clone(), "Some project");
    let chore = create_test_chore_manual(&db, product.id.clone(), "Chore");
    let revision = insert_revision_row(&db, &product.id, &chore.id);

    let err = db
        .update_work_item(
            &revision,
            WorkItemPatch {
                project_id: Some(project.id.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("kind `revision` has its own project-membership semantics"),
        "expected the revision-specific refusal, got: {message}"
    );
    assert_eq!(
        task_project_id(&db, &revision),
        None,
        "a refused patch must not have mutated the revision's project_id"
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
