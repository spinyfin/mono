use super::*;

#[test]
fn followup_moves_preserve_provenance_and_project_views() {
    let path = temp_db_path("followup-project-moves");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let mut projects = Vec::new();
    for name in ["First", "Second"] {
        projects.push(
            db.create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: name.to_owned(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: true,
                design_reasoning_effort_xhigh: false,
            })
            .unwrap(),
        );
    }
    db.create_task(
        CreateTaskInput::builder()
            .product_id(product.id.clone())
            .project_id(projects[1].id.clone())
            .name("Existing member")
            .build(),
    )
    .unwrap();
    let origin = create_test_chore_manual(&db, product.id.clone(), "Origin");
    db.update_work_item(
        &origin.id,
        WorkItemPatch {
            pr_url: Some("https://github.com/org/repo/pull/117".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let followup = db
        .create_review_findings_followup(
            review_findings_followup::ReviewFindingsFollowupInsert::builder()
                .product_id(product.id.clone())
                .name("Review findings")
                .created_via("pr_review:test-membership")
                .description("Fix the finding")
                .chain_root_id(origin.id.clone())
                .autostart(false)
                .build(),
        )
        .unwrap();
    assert_eq!(followup.kind, TaskKind::Followup);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET review_cycle = 2, last_reviewed_sha = 'reviewed-head' WHERE id = ?1",
            [&followup.id],
        )
        .unwrap();
    let revision = insert_revision_row(&db, &product.id, &followup.id);

    for (target, ordinal) in [
        (Some(&projects[0].id), Some(1)),
        (Some(&projects[1].id), Some(2)),
        (None, None),
    ] {
        let updated = db
            .update_work_item(
                &followup.id,
                WorkItemPatch {
                    project_id: Some(target.cloned().unwrap_or_default()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(matches!(updated, WorkItem::Chore(_)), target.is_none());
        let task = match updated {
            WorkItem::Task(task) | WorkItem::Chore(task) => task,
            other => panic!("expected followup, got {other:?}"),
        };
        assert_eq!(task.kind, TaskKind::Followup);
        assert_eq!(task.project_id.as_ref(), target);
        assert_eq!(task.ordinal, ordinal);
        assert_eq!(task.short_id, followup.short_id);
        assert_eq!(task.origin_task_short_id, origin.short_id);
        assert_eq!(task.origin_pr_number, Some(117));
        assert_eq!(task.created_via, followup.created_via);
        assert_eq!(task.description, followup.description);
        assert_eq!(task.parent_task_id, followup.parent_task_id);
        assert_eq!(task.review_cycle, 2);
        assert_eq!(task.last_reviewed_sha.as_deref(), Some("reviewed-head"));
        let WorkItem::Task(child) = db.get_work_item(&revision).unwrap() else {
            panic!("expected revision")
        };
        assert_eq!(child.project_id.as_ref(), target);
        for project in &projects {
            let listed = db.list_tasks(&product.id, Some(&project.id), None, false).unwrap();
            assert_eq!(
                listed.iter().any(|row| row.id == followup.id),
                target == Some(&project.id)
            );
        }
        let tree = db.get_work_tree(&product.id).unwrap();
        assert_eq!(tree.tasks.iter().any(|row| row.id == followup.id), target.is_some());
        assert_eq!(tree.chores.iter().any(|row| row.id == followup.id), target.is_none());
        let chores = db.list_chores(&product.id, None, false).unwrap();
        assert_eq!(chores.iter().any(|row| row.id == followup.id), target.is_none());
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn followups_in_a_project_get_distinct_ordinals_and_reorder() {
    let path = temp_db_path("followup-project-ordinals");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Ordered".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let origin = create_test_chore_manual(&db, product.id.clone(), "Origin");
    db.update_work_item(
        &origin.id,
        WorkItemPatch {
            pr_url: Some("https://github.com/org/repo/pull/117".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut followup_ids = Vec::new();
    for name in ["Followup A", "Followup B"] {
        let followup = db
            .create_review_findings_followup(
                review_findings_followup::ReviewFindingsFollowupInsert::builder()
                    .product_id(product.id.clone())
                    .name(name)
                    .created_via(format!("pr_review:{name}"))
                    .description("Fix the finding")
                    .chain_root_id(origin.id.clone())
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        assert_eq!(followup.kind, TaskKind::Followup);
        followup_ids.push(followup.id);
    }
    let move_into = |id: &str| {
        db.update_work_item(
            id,
            WorkItemPatch {
                project_id: Some(project.id.clone()),
                ..Default::default()
            },
        )
        .unwrap()
    };
    for id in &followup_ids {
        let WorkItem::Task(moved) = move_into(id) else {
            panic!("expected project-bound followup")
        };
        assert_eq!(moved.kind, TaskKind::Followup);
    }
    let regular = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Regular")
                .build(),
        )
        .unwrap();
    let chore = create_test_chore_manual(&db, product.id.clone(), "Moved regular");
    let WorkItem::Task(moved_regular) = move_into(&chore.id) else {
        panic!("expected moved task")
    };
    assert_eq!(moved_regular.kind, TaskKind::ProjectTask);

    let ordinal_of = |id: &str| {
        db.list_tasks(&product.id, Some(&project.id), None, false)
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .and_then(|row| row.ordinal)
            .unwrap()
    };
    let (a, b, r, m) = (
        ordinal_of(&followup_ids[0]),
        ordinal_of(&followup_ids[1]),
        ordinal_of(&regular.id),
        ordinal_of(&chore.id),
    );
    assert!(a < b && b < r && r < m, "ordinals must be increasing: {a} {b} {r} {m}");

    let reordered = vec![
        chore.id.clone(),
        followup_ids[1].clone(),
        regular.id.clone(),
        followup_ids[0].clone(),
    ];
    db.reorder_project_tasks(&project.id, &reordered).unwrap();
    let listed: Vec<String> = db
        .list_tasks(&product.id, Some(&project.id), None, false)
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(listed, reordered);
    for id in &followup_ids {
        let rows = db.list_tasks(&product.id, Some(&project.id), None, false).unwrap();
        let row = rows.iter().find(|row| &row.id == id).unwrap();
        assert_eq!(row.kind, TaskKind::Followup);
    }
    let _ = std::fs::remove_file(path);
}
