use super::*;

fn setup_revision_chain(db: &WorkDb, label: &str) -> (Product, Task, Task) {
    let product = create_test_product_named(db, &format!("RevRedispatch-{label}"));
    let root = create_test_chore(db, product.id.clone(), "Parent PR chore");
    db.update_work_item(
        &root.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some("https://github.com/spinyfin/mono/pull/9999".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let rev = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(root.id.clone())
                .name("Revision 1")
                .description("Rev desc")
                .build(),
            &checker,
        )
        .unwrap();
    // `create_revision` mints an initial `ready` execution; clear it so each
    // test controls the exact execution history on `rev`.
    db.connect()
        .unwrap()
        .execute("DELETE FROM work_executions WHERE work_item_id = ?1", params![rev.id])
        .unwrap();
    (product, root, rev)
}

#[test]
fn completed_revision_is_not_reminted_by_reconcile_revision_execution() {
    let db = WorkDb::open(temp_db_path("completed-rev-no-remint")).unwrap();
    let (product, _root, rev) = setup_revision_chain(&db, "no-remint");

    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(rev.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    assert_eq!(exec.created_via.as_deref(), Some("test"));

    let result = db.reconcile_product_executions(&product.id).unwrap();
    assert!(
        !result.created.iter().any(|e| e.work_item_id == rev.id),
        "reconcile must not mint a new execution for a completed revision"
    );
    let execs = db.list_executions(Some(&rev.id)).unwrap();
    assert_eq!(execs.len(), 1);
}

#[test]
fn abandoned_duplicate_after_completed_revision_does_not_enable_redispatch() {
    let db = WorkDb::open(temp_db_path("abandoned-dup-after-completed")).unwrap();
    let (product, _root, rev) = setup_revision_chain(&db, "abandoned-dup");

    let completed = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("reconcile_revision_execution")
                .work_item_id(rev.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    let abandoned = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("redundant_mint")
                .work_item_id(rev.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Abandoned)
                .build(),
        )
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '1000' WHERE id = ?1",
            params![completed.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '2000' WHERE id = ?1",
            params![abandoned.id],
        )
        .unwrap();

        assert!(
            crate::dispatch_admission::revision_implementation_completed(&conn, &rev.id).unwrap(),
            "an abandoned duplicate after a completed implementation must not shadow completion"
        );
        assert!(
            crate::dispatch_admission::delivered_or_completed(&conn, &rev.id).unwrap(),
            "delivered_or_completed must ignore abandoned duplicate rows"
        );
        let held = crate::dispatch_admission::delivered_or_completed_among(&conn, &[rev.id.as_str()]).unwrap();
        assert!(held.contains(&rev.id));
    }

    let result = db.reconcile_product_executions(&product.id).unwrap();
    assert!(
        !result.created.iter().any(|e| e.work_item_id == rev.id),
        "reconcile must not remint when a completed revision has an abandoned duplicate"
    );
}

#[test]
fn explicit_start_reopens_completed_revision_with_provenance() {
    let db = WorkDb::open(temp_db_path("explicit-start-reopens")).unwrap();
    let (_product, _root, rev) = setup_revision_chain(&db, "explicit-start");

    db.create_execution(
        CreateExecutionInput::builder()
            .created_via("reconcile_revision_execution")
            .work_item_id(rev.id.clone())
            .kind(ExecutionKind::RevisionImplementation)
            .status(ExecutionStatus::Completed)
            .build(),
    )
    .unwrap();

    let reopened = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(rev.id.clone())
                .explicit_start(true)
                .created_via("frontend:request_execution")
                .build(),
        )
        .unwrap();
    assert_eq!(reopened.status, ExecutionStatus::Ready);
    assert_eq!(reopened.created_via.as_deref(), Some("frontend:request_execution"));

    let conn = db.connect().unwrap();
    assert!(
        !crate::dispatch_admission::revision_implementation_completed(&conn, &rev.id).unwrap(),
        "explicit start creates a newer ready execution so revision is no longer completed"
    );
}

#[test]
fn reopened_in_review_prerequisite_revision_gates_dependent_revision() {
    let db = WorkDb::open(temp_db_path("prereq-reopen-gates")).unwrap();
    let (_product, _root, rev1) = setup_revision_chain(&db, "prereq-gate");

    // rev1 delivered and is in_review.
    db.create_execution(
        CreateExecutionInput::builder()
            .created_via("reconcile_revision_execution")
            .work_item_id(rev1.id.clone())
            .kind(ExecutionKind::RevisionImplementation)
            .status(ExecutionStatus::Completed)
            .build(),
    )
    .unwrap();
    db.update_work_item(
        &rev1.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // rev2 depends on rev1.
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let rev2 = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(rev1.id.clone())
                .name("Revision 2")
                .description("Depends on rev1")
                .build(),
            &checker,
        )
        .unwrap();
    db.add_dependency(AddDependencyInput {
        dependent: rev2.id.clone(),
        prerequisite: rev1.id.clone(),
        relation: Some("blocks".to_owned()),
    })
    .unwrap();

    assert!(
        db.gating_prereqs_for(&rev2.id).unwrap().is_empty(),
        "in_review rev1 with no live implementation satisfies rev2"
    );

    // Reopen rev1 while its task row still says in_review.
    let reopened = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("frontend:request_execution")
                .work_item_id(rev1.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    assert_eq!(
        db.gating_prereqs_for(&rev2.id).unwrap(),
        vec![rev1.id.clone()],
        "reopened in_review prerequisite revision must gate its dependent"
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            params![reopened.id],
        )
        .unwrap();
    assert!(
        db.gating_prereqs_for(&rev2.id).unwrap().is_empty(),
        "once rev1's reopened execution completes, rev2 is unblocked again"
    );
}

#[test]
fn list_task_field_audit_with_status_surfaces_engine_status_reasons() {
    let db = WorkDb::open(temp_db_path("status-audit-with-status")).unwrap();
    let product = create_test_product_named(&db, "StatusAudit");
    let chore = create_test_chore(&db, product.id.clone(), "Audited chore");

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    assert!(db.demote_active_work_item_to_todo(&chore.id).unwrap());

    let flags_only = db.list_task_field_audit(&chore.id).unwrap();
    assert!(
        !flags_only.iter().any(|row| {
            row.property == "status"
                && row.old_value.as_deref() == Some("active")
                && row.new_value.as_deref() == Some("todo")
        }),
        "default list_task_field_audit omits standalone status transitions"
    );

    let with_status = db.list_task_field_audit_with_status(&chore.id, true).unwrap();
    let demote = with_status
        .iter()
        .rfind(|row| row.property == "status")
        .expect("status transition should be present when include_status=true");
    assert_eq!(demote.old_value.as_deref(), Some("active"));
    assert_eq!(demote.new_value.as_deref(), Some("todo"));
    assert_eq!(demote.actor, "engine");
    assert_eq!(demote.basis, "demote_active_work_item_to_todo");
}
