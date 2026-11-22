use super::*;

#[test]
fn reconciliation_history_cannot_enter_worker_prompts() {
    use crate::test_support::{create_test_chore_manual, create_test_product_named};
    let dir = tempfile::tempdir().unwrap();
    let db = crate::work::WorkDb::open(dir.path().join("test.db")).unwrap();
    let product = create_test_product_named(&db, "prompt isolation");
    let chore = create_test_chore_manual(&db, product.id, "Brief");
    let brief = "Implement the widget importer.\nPreserve its public API.";
    db.update_work_item(
        &chore.id,
        crate::work::WorkItemPatch {
            description: Some(brief.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let audit = "[engine-reconcile] epoch 1700000000: dead worker detected via PID probe; reset for redispatch. /tmp/historical.patch";
    for epoch in 0..45 {
        db.record_reconcile_audit(&chore.id, epoch, audit).unwrap();
    }
    let work_item = db.get_work_item(&chore.id).unwrap();
    let WorkItem::Chore(task) = &work_item else {
        panic!("chore")
    };
    assert_eq!(
        task.description, brief,
        "no prompt-time filtering may hide accumulation"
    );
    assert_eq!(
        db.list_task_field_audit(&chore.id)
            .unwrap()
            .iter()
            .filter(|row| row.property == "reconciliation")
            .count(),
        45
    );
    for kind in [
        ExecutionKind::TaskImplementation,
        ExecutionKind::ChoreImplementation,
        ExecutionKind::RevisionImplementation,
        ExecutionKind::InvestigationImplementation,
    ] {
        let mut execution = base_execution();
        execution.kind = kind;
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .review_signal_checks(|_| &[])
                .execution(&execution)
                .work_item(&work_item)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(prompt.contains(brief));
        for bookkeeping in [
            "[engine-reconcile]",
            "1700000000",
            "PID probe",
            "historical.patch",
            "reset for redispatch",
        ] {
            assert!(
                !prompt.contains(bookkeeping),
                "{:?} leaked {bookkeeping}",
                execution.kind
            );
        }
    }
}
