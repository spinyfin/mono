use super::*;
use crate::test_support::{
    create_active_chore, create_product, open_db, seed_daily_automation, seed_review_guide_series,
};
use boss_protocol::ExecutionQueryDimension as Dim;

fn execution(db: &WorkDb, task: &str, created_at: &str) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
            params![execution.id, created_at],
        )
        .unwrap();
    execution.id
}

#[test]
fn filters_compose_and_preserve_recorded_values_and_task_labels() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let task = create_active_chore(&db, &product, "query filters");
    let matching = execution(&db, &task, "100");
    execution(&db, &task, "99");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET reasoning = 'investigation', effort_level = 'small', deleted_at = '101' WHERE id = ?1",
            [&task],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_via = 'progression_sweep', driver = 'codex',
         model = 'model-x', effort_level = 'large', last_error = 'literal %_ error',
         dispatch_wait_reason = 'pool_exhausted', progression_disposition = 'progressed'
         WHERE id = ?1",
            [&matching],
        )
        .unwrap();
    let query = ExecutionQuery::builder()
        .created_via(vec!["progression_sweep".into()])
        .kind(vec!["chore_implementation".into()])
        .status(vec!["ready".into(), "completed".into()])
        .driver(vec!["codex".into()])
        .model(vec!["model-x".into()])
        .reasoning(vec!["investigation".into()])
        .effort(vec!["large".into()])
        .since_epoch_s(100)
        .product(&product)
        .error_match("%_")
        .build();
    let result = db.query_executions(&query).unwrap();
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    assert_eq!(row.id, matching);
    assert_eq!(row.owning_task_id.as_deref(), Some(task.as_str()));
    assert_eq!(
        row.work_item_short_id,
        query_task(&db.connect().unwrap(), &task)
            .unwrap()
            .unwrap()
            .short_id
            .map(|n| format!("T{n}"))
    );
    assert_eq!(row.product_slug.as_deref(), Some("test-product"));
    assert_eq!(row.created_at_epoch_s, 100);
    assert_eq!(row.dispatch_wait_reason.as_deref(), Some("pool_exhausted"));
    assert_eq!(row.progression_disposition.as_deref(), Some("progressed"));
    assert_eq!(row.effort.as_deref(), Some("large"));
    let by_slug = ExecutionQuery {
        product: Some("test-product".into()),
        ..query.clone()
    };
    assert_eq!(db.query_executions(&by_slug).unwrap().rows.len(), 1);
    for field in [
        "created_via",
        "kind",
        "status",
        "driver",
        "model",
        "reasoning",
        "effort",
        "product",
        "error_match",
    ] {
        let mut value = serde_json::to_value(&query).unwrap();
        value[field] = if matches!(field, "product" | "error_match") {
            serde_json::json!("no match' OR 1=1 --")
        } else {
            serde_json::json!(["no match' OR 1=1 --"])
        };
        let changed = serde_json::from_value(value).unwrap();
        assert!(db.query_executions(&changed).unwrap().rows.is_empty(), "{field}");
    }
    assert!(
        db.query_executions(&ExecutionQuery {
            since_epoch_s: Some(101),
            ..query
        })
        .unwrap()
        .rows
        .is_empty()
    );
}

#[test]
fn cross_tabs_preserve_nulls_and_count_executions_not_runs() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let task = create_active_chore(&db, &product, "cross tabs");
    let first = execution(&db, &task, "9");
    let second = execution(&db, &task, "10");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_reason = '' WHERE id = ?1",
            [&second],
        )
        .unwrap();
    for id in ["run-a", "run-b"] {
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO work_runs (id, execution_id, agent_id, status, error_text, created_at)
             VALUES (?1, ?2, 'worker', 'failed', 'network %_ failure', '10')",
                params![id, first],
            )
            .unwrap();
    }
    let all = db.query_executions(&ExecutionQuery::default()).unwrap();
    assert_eq!(all.rows.iter().map(|r| &r.id).collect::<Vec<_>>(), [&second, &first]);
    let query = ExecutionQuery::builder().group_by(Dim::ALL.to_vec()).build();
    let result = db.query_executions(&query).unwrap();
    assert!(result.rows.is_empty());
    assert_eq!(result.groups.len(), 2);
    assert_eq!(result.groups.iter().map(|g| g.count).sum::<u64>(), 2);
    assert_eq!(result.groups[0].keys["dispatch_wait_reason"], None);
    assert_eq!(result.groups[1].keys["dispatch_wait_reason"].as_deref(), Some(""));
    assert_eq!(result.groups[0].keys["model"], None);
    let filtered = ExecutionQuery {
        error_match: Some("%_".into()),
        model: vec!["(unset)".into()],
        ..query
    };
    let result = db.query_executions(&filtered).unwrap();
    assert_eq!(result.groups.len(), 1);
    assert_eq!(result.groups[0].count, 1);
    let repeated = ExecutionQuery::builder()
        .group_by(vec![Dim::Status, Dim::Status])
        .build();
    let result = db.query_executions(&repeated).unwrap();
    assert_eq!(result.group_by, [Dim::Status]);
    assert_eq!(result.groups[0].count, 2);
}

#[test]
fn review_guides_resolve_owner_and_unbound_executions_remain_visible() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let task = create_active_chore(&db, &product, "guide ownership");
    let (_, comparison) = seed_review_guide_series(&db, &task);
    let guide = db.create_pr_review_guide_execution(&comparison, "test/repo").unwrap();
    let orphan = execution(&db, &task, "10");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET work_item_id = 'missing-owner' WHERE id = ?1",
            [&orphan],
        )
        .unwrap();
    let rows = db.query_executions(&ExecutionQuery::default()).unwrap().rows;
    assert_eq!(rows.len(), 2);
    let row = rows.iter().find(|r| r.id == guide.id).unwrap();
    assert_eq!(row.work_item_id, comparison);
    assert_eq!(row.owning_task_id.as_deref(), Some(task.as_str()));
    assert!(row.work_item_short_id.is_some());
    assert_eq!(row.product_slug.as_deref(), Some("test-product"));
    let row = rows.iter().find(|r| r.id == orphan).unwrap();
    assert!(row.product_slug.is_none());
    assert!(row.work_item_short_id.is_none());
    let result = db
        .query_executions(&ExecutionQuery::builder().product(product).build())
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].id, guide.id);
    let empty = db
        .query_executions(
            &ExecutionQuery::builder()
                .model(vec!["missing".into()])
                .group_by(vec![Dim::Status])
                .build(),
        )
        .unwrap();
    assert!(empty.groups.is_empty());
}

#[test]
fn product_filter_also_includes_product_project_and_automation_executions() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(&product)
                .name("execution ownership")
                .autostart(false)
                .no_design_task(true)
                .build(),
        )
        .unwrap();
    let automation = seed_daily_automation(&db, &product);
    let triage = db
        .create_automation_triage_execution(&automation.id, "test/repo")
        .unwrap();
    let mut expected = vec![triage.id];
    for (id, kind) in [
        (&product, ExecutionKind::ProductDesign),
        (&project.id, ExecutionKind::ProjectDesign),
    ] {
        expected.push(
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(id)
                    .kind(kind)
                    .repo_remote_url("https://github.com/test/repo")
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap()
            .id,
        );
    }
    let rows = db
        .query_executions(&ExecutionQuery::builder().product(&product).build())
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 3);
    for row in rows {
        assert!(expected.contains(&row.id));
        assert_eq!(row.product_slug.as_deref(), Some("test-product"));
        assert!(row.work_item_short_id.is_none());
        assert!(row.owning_task_id.is_none());
    }
}
