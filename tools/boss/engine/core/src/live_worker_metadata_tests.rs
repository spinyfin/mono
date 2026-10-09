use super::*;

use boss_protocol::{CreateProjectInput, CreateTaskInput, ExecutionKind, RequestExecutionInput};

use crate::test_support::{
    create_test_chore_manual, create_test_product, insert_host_capability, open_db, seed_daily_automation,
};

/// Request and start an execution of `work_item_id` on `host_id`, so it
/// carries a `started_at`.
fn started_execution(db: &WorkDb, work_item_id: &str, host_id: &str) -> WorkExecution {
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
        .unwrap();
    let (execution, _run) = db
        .start_execution_run_on_host_with_tmux_hosting(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "ws-1",
            "/tmp/ws-1",
            host_id,
            host_id == "local",
        )
        .unwrap();
    execution
}

fn create_project(db: &WorkDb, product_id: &str, name: &str) -> boss_protocol::Project {
    db.create_project(
        CreateProjectInput::builder()
            .product_id(product_id)
            .name(name)
            .no_design_task(true)
            .autostart(false)
            .build(),
    )
    .unwrap()
}

fn create_project_task(db: &WorkDb, product_id: &str, project_id: &str, name: &str) -> boss_protocol::Task {
    insert_host_capability(db, "local", "driver=claude", "auto");
    db.create_task(
        CreateTaskInput::builder()
            .product_id(product_id)
            .project_id(project_id)
            .name(name)
            .autostart(false)
            .build(),
    )
    .unwrap()
}

/// The same execution, relabelled as if it were dispatched as `kind`.
fn as_kind(execution: &WorkExecution, kind: ExecutionKind) -> WorkExecution {
    WorkExecution {
        kind,
        ..execution.clone()
    }
}

#[test]
fn project_task_is_attributed_to_its_project() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let project = create_project(&db, &product.id, "Dynamic Agents");
    let task = create_project_task(&db, &product.id, &project.id, "stamp metadata");
    let execution = started_execution(&db, &task.id, "local");

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.project_id.as_deref(), Some(project.id.as_str()));
    assert_eq!(metadata.project_name.as_deref(), Some("Dynamic Agents"));
}

#[test]
fn project_work_item_is_attributed_to_itself() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let project = create_project(&db, &product.id, "Whole project");
    let execution = as_kind(
        &started_execution(&db, &create_test_chore_manual(&db, product.id.clone(), "x").id, "local"),
        ExecutionKind::ProjectDesign,
    );
    let execution = WorkExecution {
        work_item_id: project.id.clone(),
        ..execution
    };

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.project_id.as_deref(), Some(project.id.as_str()));
    assert_eq!(metadata.project_name.as_deref(), Some("Whole project"));
    assert_eq!(metadata.agent_type.as_deref(), Some("design"));
}

#[test]
fn unfiled_chore_has_no_project() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "loose chore");
    let execution = started_execution(&db, &chore.id, "local");

    let metadata = resolve(&db, &execution, "local");

    assert_eq!(metadata.project_id, None);
    assert_eq!(metadata.project_name, None);
}

#[test]
fn work_item_ids_that_are_not_tasks_are_unfiled_not_guessed() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let automation = seed_daily_automation(&db, &product.id);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let base = started_execution(&db, &chore.id, "local");

    // Automation triage and review-guide executions point `work_item_id` at an
    // automation / comparison id, which is not a work item at all.
    let triage = WorkExecution {
        work_item_id: automation.id.clone(),
        ..as_kind(&base, ExecutionKind::AutomationTriage)
    };
    let guide = WorkExecution {
        work_item_id: "cmp_not_a_work_item".to_owned(),
        ..as_kind(&base, ExecutionKind::PrReviewGuide)
    };

    for execution in [triage, guide] {
        let metadata = resolve(&db, &execution, "local");
        assert_eq!(metadata.project_id, None, "{}", execution.kind);
        assert_eq!(metadata.project_name, None, "{}", execution.kind);
    }
}

#[test]
fn automation_sourced_project_task_keeps_its_project_but_is_typed_automation() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let project = create_project(&db, &product.id, "Maintenance");
    let task = create_project_task(&db, &product.id, &project.id, "automation produced");
    let automation = seed_daily_automation(&db, &product.id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, task.id],
        )
        .unwrap();
    let execution = started_execution(&db, &task.id, "local");

    let coding = resolve(&db, &execution, "local");
    assert_eq!(coding.agent_type.as_deref(), Some("automation"));
    assert_eq!(coding.project_id.as_deref(), Some(project.id.as_str()));

    // Review precedence beats the automation source.
    let review = resolve(&db, &as_kind(&execution, ExecutionKind::PrReview), "local");
    assert_eq!(review.agent_type.as_deref(), Some("review"));
    assert_eq!(review.project_id.as_deref(), Some(project.id.as_str()));
}

#[test]
fn every_execution_kind_resolves_a_badge_type() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let base = started_execution(&db, &chore.id, "local");

    let expected = [
        (ExecutionKind::AnswerAgent, "answer"),
        (ExecutionKind::AutomationTriage, "automation"),
        (ExecutionKind::ChoreImplementation, "coding"),
        (ExecutionKind::CiRemediation, "coding"),
        (ExecutionKind::ConflictResolution, "coding"),
        (ExecutionKind::InvestigationImplementation, "coding"),
        (ExecutionKind::PrReview, "review"),
        (ExecutionKind::PrReviewGuide, "review"),
        (ExecutionKind::ProductDesign, "design"),
        (ExecutionKind::ProjectDesign, "design"),
        (ExecutionKind::RevisionImplementation, "coding"),
        (ExecutionKind::TaskImplementation, "coding"),
    ];
    assert_eq!(expected.len(), 12);
    for (kind, badge) in expected {
        let metadata = resolve(&db, &as_kind(&base, kind.clone()), "local");
        assert_eq!(metadata.agent_type.as_deref(), Some(badge), "{kind}");
    }
}

#[test]
fn host_is_explicit_and_distinguishes_local_from_remote() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let execution = started_execution(&db, &chore.id, "local");

    assert_eq!(resolve(&db, &execution, "local").host_id.as_deref(), Some("local"));
    assert_eq!(resolve(&db, &execution, "zakalwe").host_id.as_deref(), Some("zakalwe"));
}

#[test]
fn started_at_is_the_execution_start_as_sortable_iso8601() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let execution = started_execution(&db, &chore.id, "local");
    let epoch = execution.started_epoch().expect("a started execution has started_at");

    let metadata = resolve(&db, &execution, "local");
    assert_eq!(
        metadata.started_at.as_deref(),
        Some(boss_engine_utils::iso8601::format_epoch_iso8601(epoch).as_str()),
    );

    let unstarted = WorkExecution {
        started_at: None,
        ..execution
    };
    assert_eq!(resolve(&db, &unstarted, "local").started_at, None);
}
