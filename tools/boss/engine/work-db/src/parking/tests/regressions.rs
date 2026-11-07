use super::*;
use boss_protocol::OperatorQuestionView;

#[test]
fn bulk_dispatch_skips_parked_and_human_rows_without_losing_other_work() {
    for startup in [true, false] {
        let (_dir, db, product) = fixture();
        let parked = create_active_chore(&db, &product, "parked");
        let eligible = create_active_chore(&db, &product, "eligible");
        let human = create_active_chore(&db, &product, "human");
        for id in [&parked, &eligible, &human] {
            let execution = create_ready_chore_execution(&db, id.clone());
            db.connect_new()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                    params![execution.id, if startup { "orphaned" } else { "cancelled" }],
                )
                .unwrap();
        }
        park(&db, &parked);
        let conn = db.connect_new().unwrap();
        conn.execute(
            "UPDATE tasks SET autostart = 1 WHERE id IN (?1, ?2, ?3)",
            params![parked, eligible, human],
        )
        .unwrap();
        conn.execute("UPDATE tasks SET human_driven = 1 WHERE id = ?1", [&human])
            .unwrap();
        let dispatched = if startup {
            db.reconcile_active_dispatch(|_| false).unwrap()
        } else {
            db.rescan_active_dispatch().unwrap()
        };
        assert_eq!(dispatched, vec![eligible.clone()]);
        assert_eq!(
            query_latest_execution_for_work_item(&conn, &eligible)
                .unwrap()
                .unwrap()
                .status,
            ExecutionStatus::Ready
        );
        assert!(
            query_latest_execution_for_work_item(&conn, &parked)
                .unwrap()
                .unwrap()
                .status
                .is_terminal()
        );
    }
}

#[test]
fn revision_keeps_direct_parent_stamp_after_project_unpark() {
    let (_dir, db, product) = fixture();
    // Reproduce an already-deployed database whose original trigger exists.
    let conn = db.connect_new().unwrap();
    conn.execute_batch("DROP TRIGGER tasks_born_parked_in_parked_project;")
        .unwrap();
    conn.execute_batch(PARKING_MEMBERSHIP_TRIGGERS).unwrap();
    migrate_membership(&conn).unwrap();
    migrate_membership(&conn).unwrap();
    let project = project(&db, &product, "parked project");
    let parent = project_task(&db, &product, &project, "parent");
    park(&db, &project);
    park(&db, &parent);
    let revision = revision_of(&db, &parent, "direct-parent");
    assert_eq!(get_task(&db, &revision).parked_via.as_deref(), Some(PARKED_VIA_DIRECT));
    assert_eq!(ledger(&db, &revision).len(), 1);
    unpark(&db, &project).unwrap();
    assert!(get_task(&db, &revision).is_parked());
    assert!(db.is_parked(&revision).unwrap());
}

#[test]
fn reopened_and_restored_project_children_receive_stamps() {
    for state in ["done", "archived", "deleted"] {
        let (_dir, db, product) = fixture();
        let project = project(&db, &product, "parked project");
        let task = project_task(&db, &product, &project, "child");
        let conn = db.connect_new().unwrap();
        if state == "deleted" {
            conn.execute("UPDATE tasks SET deleted_at = '1' WHERE id = ?1", [&task])
                .unwrap();
        } else {
            conn.execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![task, state])
                .unwrap();
        }
        park(&db, &project);
        assert!(!get_task(&db, &task).is_parked());
        conn.execute(
            "UPDATE tasks SET status = 'todo', deleted_at = NULL WHERE id = ?1",
            [&task],
        )
        .unwrap();
        assert!(get_task(&db, &task).is_parked());
        assert!(db.is_parked(&task).unwrap());
    }
}

fn question() -> OperatorQuestionView {
    OperatorQuestionView::builder()
        .id("question")
        .text("Proceed?")
        .answer_type(boss_protocol::OperatorAnswerType::YesNo)
        .explanation("Resume the work.")
        .asked_at("2026-11-06")
        .execution_id("asking")
        .build()
}

#[test]
fn parked_answer_preserves_description_without_minting() {
    let (_dir, db, product) = fixture();
    let task = create_test_chore_manual(&db, &product, "answer");
    park(&db, &task.id);
    let mut conn = db.connect_new().unwrap();
    let tx = conn.transaction().unwrap();
    let restarted = crate::pr_flow::restart_after_answer(
        &mut PendingEvents::new(),
        &tx,
        &task,
        &question(),
        "\nRecorded answer: Yes",
        "1",
    )
    .unwrap();
    assert!(!restarted);
    tx.commit().unwrap();
    let row = get_task(&db, &task.id);
    assert!(row.description.contains("Recorded answer: Yes"));
    assert_eq!(row.status, TaskStatus::Todo);
    assert!(query_latest_execution_for_work_item(&conn, &task.id).unwrap().is_none());
}

#[test]
fn approval_links_parked_prerequisite_without_minting() {
    let (_dir, db, product) = fixture();
    let task = create_test_chore_manual(&db, &product, "dependent");
    let prerequisite = create_test_chore_manual(&db, &product, "prerequisite");
    park(&db, &prerequisite.id);
    park(&db, &task.id);
    let mut conn = db.connect_new().unwrap();
    let tx = conn.transaction().unwrap();
    let linked = crate::prerequisite_question::approve_in_tx(
        &mut PendingEvents::new(),
        &tx,
        &task,
        &question(),
        ("prerequisite", "Fix prerequisite"),
        chrono::DateTime::from_timestamp(1, 0).unwrap(),
        "1",
    )
    .unwrap();
    assert_eq!(linked.id, prerequisite.id);
    assert!(!linked.created);
    tx.commit().unwrap();
    assert!(get_task(&db, &task.id).description.contains("Prerequisite approved"));
    for id in [&task.id, &prerequisite.id] {
        assert!(query_latest_execution_for_work_item(&conn, id).unwrap().is_none());
    }
}
