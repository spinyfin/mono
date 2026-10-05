use super::*;
use crate::test_support::{create_test_product_named, open_db};

fn empty_project(db: &WorkDb) -> Project {
    let product = create_test_product_named(db, "Postmortems");
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id)
                .name("Completed work")
                .no_design_task(true)
                .build(),
        )
        .unwrap();
    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_path: Some("docs/design.md".into()),
        ..Default::default()
    })
    .unwrap();
    query_project(&db.connect().unwrap(), &project.id).unwrap().unwrap()
}

/// A project with one completed implementation task. The completion signal
/// that task recorded is cleared so each test exercises its own trigger.
fn project(db: &WorkDb) -> Project {
    let p = empty_project(db);
    let done = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(&p.product_id)
                .project_id(&p.id)
                .name("Completed implementation")
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.update_work_item(
        &done.id,
        WorkItemPatch {
            status: Some("done".into()),
            pr_url: Some("https://github.com/o/r/pull/1".into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute("DELETE FROM project_postmortem_signals", [])
        .unwrap();
    p
}

fn task(db: &WorkDb, project: &Project) -> Task {
    db.create_task(
        CreateTaskInput::builder()
            .product_id(&project.product_id)
            .project_id(&project.id)
            .name("Last open work")
            .autostart(false)
            .build(),
    )
    .unwrap()
}

async fn assert_scheduled_once(db: &WorkDb, project: &Project) {
    assert_eq!(
        crate::project_postmortem_sweep::run_one_pass(db)
            .await
            .postmortems_created,
        1
    );
    assert_eq!(
        crate::project_postmortem_sweep::run_one_pass(db)
            .await
            .postmortems_created,
        0
    );
    assert!(!db.start_project_postmortem(&project.id).unwrap().1);
    let count: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE project_id = ?1 AND kind = 'design_postmortem'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let executions: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM work_executions e JOIN tasks t ON t.id = e.work_item_id
         WHERE t.project_id = ?1 AND t.kind = 'design_postmortem'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(executions, 1, "completion must also enqueue the postmortem execution");
}

#[tokio::test]
async fn closing_last_task_schedules_even_in_watermark_second() {
    let (_dir, db) = open_db();
    let p = project(&db);
    let t = task(&db, &p);
    // Signals, unlike timestamp comparisons, cannot lose same-second edges.
    db.set_metadata("project_postmortem_sweep_watermark", &now_string())
        .unwrap();
    db.update_work_item(
        &t.id,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_scheduled_once(&db, &p).await;
}

#[tokio::test]
async fn moving_last_task_out_schedules_old_project() {
    let (_dir, db) = open_db();
    let p = project(&db);
    let t = task(&db, &p);
    db.update_work_item(
        &t.id,
        WorkItemPatch {
            project_id: Some(String::new()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_scheduled_once(&db, &p).await;
}

#[tokio::test]
async fn deleting_last_task_schedules_project() {
    let (_dir, db) = open_db();
    let p = project(&db);
    let t = task(&db, &p);
    db.delete_work_item(&t.id).unwrap();
    assert_scheduled_once(&db, &p).await;
}

#[tokio::test]
async fn archiving_last_task_schedules_project() {
    let (_dir, db) = open_db();
    let p = project(&db);
    let t = task(&db, &p);
    db.update_work_item(
        &t.id,
        WorkItemPatch {
            status: Some("archived".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_scheduled_once(&db, &p).await;
}

#[tokio::test]
async fn explicit_project_done_schedules_and_survives_restart() {
    let (dir, db) = open_db();
    let p = project(&db);
    db.update_work_item(
        &p.id,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    drop(db);
    let db = WorkDb::open(dir.path().join("state.db")).unwrap();
    assert_scheduled_once(&db, &p).await;
}

#[tokio::test]
async fn done_override_does_not_hide_open_work() {
    let (_dir, db) = open_db();
    let p = project(&db);
    task(&db, &p);
    db.update_work_item(
        &p.id,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        crate::project_postmortem_sweep::run_one_pass(&db)
            .await
            .postmortems_created,
        0
    );
    assert!(
        db.start_project_postmortem(&p.id)
            .unwrap_err()
            .to_string()
            .contains("1 open task")
    );
}

#[test]
fn planning_blocks_postmortem_until_materialization_finishes() {
    let (_dir, db) = open_db();
    let p = project(&db);
    db.claim_planner_run(ClaimPlannerRunInput {
        project_id: &p.id,
        product_id: &p.product_id,
        design_task_id: None,
        caller: "operator",
    })
    .unwrap()
    .unwrap();
    assert!(
        db.start_project_postmortem(&p.id)
            .unwrap_err()
            .to_string()
            .contains("planning or staged work")
    );
}

#[tokio::test]
async fn moving_directly_to_another_project_preserves_old_completion_signal() {
    let (_dir, db) = open_db();
    let p = project(&db);
    let destination = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(&p.product_id)
                .name("Next project")
                .no_design_task(true)
                .build(),
        )
        .unwrap();
    let t = task(&db, &p);
    db.update_work_item(
        &t.id,
        WorkItemPatch {
            project_id: Some(destination.id.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_scheduled_once(&db, &p).await;
    assert!(
        db.last_design_postmortem_for_project(&destination.id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn racing_requests_create_one_postmortem() {
    let (_dir, db) = open_db();
    let p = project(&db);
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let handles = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    db.start_project_postmortem(&p.id).unwrap()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1);
    assert_eq!(results[0].0.id, results[1].0.id);
}

#[tokio::test]
async fn project_done_without_completed_work_schedules_nothing() {
    let (_dir, db) = open_db();
    let p = empty_project(&db);
    db.update_work_item(
        &p.id,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(db.has_project_postmortem_signal(&p.id).unwrap());
    assert_eq!(
        crate::project_postmortem_sweep::run_one_pass(&db)
            .await
            .postmortems_created,
        0
    );
    assert!(
        db.start_project_postmortem(&p.id)
            .unwrap_err()
            .to_string()
            .contains("no implementation work completed")
    );
}

#[test]
fn command_recreates_after_postmortem_was_deleted() {
    let (_dir, db) = open_db();
    let p = project(&db);
    let (first, created) = db.start_project_postmortem(&p.id).unwrap();
    assert!(created);
    db.delete_work_item(&first.id).unwrap();
    let (second, created) = db.start_project_postmortem(&p.id).unwrap();
    assert!(created, "a deleted postmortem must not disable the command");
    assert_ne!(second.id, first.id);
}
