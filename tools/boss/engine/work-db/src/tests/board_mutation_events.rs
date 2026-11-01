use super::*;
use boss_event_bus::{Event, EventKind, TopicFilter};

async fn assert_changed(sub: &mut boss_event_bus::Subscription, task: &Task) {
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), sub.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        Event::WorkItemChanged {
            product_id: task.product_id.clone(),
            task_id: task.id.clone(),
        }
    );
}

#[tokio::test]
async fn dependency_block_and_recovery_publish_committed_card_changes() {
    let db = WorkDb::open(temp_db_path("dependency-board-events")).unwrap();
    let product = create_test_product(&db);
    let prerequisite = create_test_chore(&db, product.id.clone(), "Prerequisite");
    let dependent = create_test_chore(&db, product.id, "Dependent");
    let mut sub = db.event_bus().subscribe(TopicFilter::kind(EventKind::WorkItemChanged));
    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: prerequisite.id.clone(),
        relation: None,
    })
    .unwrap();
    assert_changed(&mut sub, &dependent).await;
    db.mark_task_done_for_test_no_cascade(&prerequisite.id).unwrap();
    assert!(db.try_unblock_dependency_if_resolved(&dependent.id).unwrap());
    assert_changed(&mut sub, &dependent).await;
    assert_eq!(
        task_from_item(db.get_work_item(&dependent.id).unwrap()).status,
        TaskStatus::Todo
    );
}

#[tokio::test]
async fn execution_request_publishes_before_dispatch_and_rollback_drops_event() {
    let db = WorkDb::open(temp_db_path("request-board-events")).unwrap();
    let product = create_test_product(&db);
    let task = create_test_chore(&db, product.id, "Request");
    let mut sub = db.event_bus().subscribe(TopicFilter::kind(EventKind::WorkItemChanged));
    db.request_execution(RequestExecutionInput::builder().work_item_id(&task.id).build())
        .unwrap();
    assert_changed(&mut sub, &task).await;
    // Refusal happens after staging, but no event may escape the failed tx.
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&task.id])
        .unwrap();
    assert!(
        db.request_execution(RequestExecutionInput::builder().work_item_id(&task.id).build())
            .is_err()
    );
    db.event_bus().publish(Event::WorkItemChanged {
        product_id: "sentinel".into(),
        task_id: "sentinel".into(),
    });
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), sub.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        Event::WorkItemChanged {
            product_id: "sentinel".into(),
            task_id: "sentinel".into()
        }
    );
}

fn task_from_item(item: WorkItem) -> Task {
    match item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        _ => panic!("expected task"),
    }
}
