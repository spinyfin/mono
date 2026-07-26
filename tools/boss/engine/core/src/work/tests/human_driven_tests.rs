//! Human-driven work-item flavor: no agent worker, manual close with summary,
//! idle/ghost-active detectors skip rows sitting in Doing by design.

use super::*;
use boss_protocol::{CreateChoreInput, RequestExecutionInput, WorkItemPatch};

fn create_human_driven_chore(db: &WorkDb, product_id: &str, name: &str) -> Task {
    db.create_chore(
        CreateChoreInput::builder()
            .product_id(product_id)
            .name(name)
            .human_driven(true)
            .autostart(true) // forced off at insert for human-driven
            .build(),
    )
    .unwrap()
}

#[test]
fn human_driven_create_forces_autostart_false() {
    let db = WorkDb::open(temp_db_path("hd-autostart")).unwrap();
    let product = create_test_product(&db);
    let chore = create_human_driven_chore(&db, &product.id, "Watch the sweep");
    assert!(chore.human_driven);
    assert!(!chore.autostart, "human-driven must never autostart");
    assert_eq!(chore.status, TaskStatus::Todo);
}

#[test]
fn human_driven_can_enter_doing_without_execution() {
    let db = WorkDb::open(temp_db_path("hd-doing")).unwrap();
    let product = create_test_product(&db);
    let chore = create_human_driven_chore(&db, &product.id, "Manual acceptance");
    let updated = db
        .update_work_item_as_actor(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
            "human",
        )
        .unwrap();
    let task = match updated {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected leaf, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::Active);
    assert!(task.human_driven);
    assert!(
        db.latest_execution_for_work_item(&chore.id).unwrap().is_none(),
        "human-driven Doing must not spawn an execution"
    );
}

#[test]
fn request_execution_refuses_human_driven() {
    let db = WorkDb::open(temp_db_path("hd-req-exec")).unwrap();
    let product = create_test_product_with_repo(&db, "p", Some("git@github.com:test/repo.git"));
    let chore = create_human_driven_chore(&db, &product.id, "No worker please");
    let err = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("human-driven"),
        "error should name human-driven; got: {msg}"
    );
}

#[test]
fn human_driven_cannot_move_to_done_without_summary() {
    let db = WorkDb::open(temp_db_path("hd-done-no-summary")).unwrap();
    let product = create_test_product(&db);
    let chore = create_human_driven_chore(&db, &product.id, "Needs summary");
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
        "human",
    )
    .unwrap();
    let err = db
        .update_work_item_as_actor(
            &chore.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
            "human",
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("completion summary") || msg.contains("boss task complete"),
        "error should require complete ritual; got: {msg}"
    );
    let still = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected leaf, got {other:?}"),
    };
    assert_eq!(still.status, TaskStatus::Active);
}

#[test]
fn human_driven_complete_with_summary_moves_to_done() {
    let db = WorkDb::open(temp_db_path("hd-complete")).unwrap();
    let product = create_test_product(&db);
    let chore = create_human_driven_chore(&db, &product.id, "Sweep row");
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
        "human",
    )
    .unwrap();
    let done = db
        .update_work_item_as_actor(
            &chore.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                completion_summary: Some("6/10 green unattended; 4 needed human intervention on CI flakes".to_owned()),
                human_driven: Some(true),
                ..WorkItemPatch::default()
            },
            "human",
        )
        .unwrap();
    let task = match done {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected leaf, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(
        task.completion_summary.as_deref(),
        Some("6/10 green unattended; 4 needed human intervention on CI flakes")
    );
    assert_eq!(task.last_status_actor, "human");
}

#[test]
fn heal_ghost_active_skips_human_driven() {
    let db = WorkDb::open(temp_db_path("hd-ghost")).unwrap();
    let product = create_test_product(&db);
    let chore = create_human_driven_chore(&db, &product.id, "Long-running human work");
    // Sit in Doing with no work_runs — exactly the ghost-active shape for
    // agent rows, but intentional for human-driven.
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
        "human",
    )
    .unwrap();
    let healed = db.heal_ghost_active_chores().unwrap();
    assert!(
        healed.iter().all(|h| h.work_item_id != chore.id),
        "human-driven active rows must not be demoted by ghost-active heal"
    );
    let still = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected leaf, got {other:?}"),
    };
    assert_eq!(still.status, TaskStatus::Active);
}

#[test]
fn mark_chore_pr_merged_skips_human_driven() {
    let db = WorkDb::open(temp_db_path("hd-merge")).unwrap();
    let product = create_test_product(&db);
    let chore = create_human_driven_chore(&db, &product.id, "Bound PR accidentally");
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some("https://github.com/spinyfin/mono/pull/99999".to_owned()),
            ..WorkItemPatch::default()
        },
        "human",
    )
    .unwrap();
    let result = db
        .mark_chore_pr_merged(&chore.id, "https://github.com/spinyfin/mono/pull/99999")
        .unwrap();
    assert!(result.is_none(), "merge poller must not close human-driven rows");
    let still = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected leaf, got {other:?}"),
    };
    assert_eq!(still.status, TaskStatus::InReview);
}

#[test]
fn work_item_is_human_driven_reads_column() {
    let db = WorkDb::open(temp_db_path("hd-column")).unwrap();
    let product = create_test_product(&db);
    let chore = create_human_driven_chore(&db, &product.id, "Column check");
    let ordinary = create_test_chore(&db, product.id.clone(), "Ordinary");
    let conn = db.connect().unwrap();
    assert!(work_item_is_human_driven(&conn, &chore.id).unwrap());
    assert!(!work_item_is_human_driven(&conn, &ordinary.id).unwrap());
}
