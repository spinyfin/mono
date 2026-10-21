//! One test per decision-terminal path that stamps cancel provenance on
//! the execution row. Each test drives the production function and
//! asserts the recorded source, actor, and reason — there is no default
//! and no placeholder.

use super::*;
use boss_protocol::CancelSource;

fn provenance_of(db: &WorkDb, execution_id: &str) -> boss_protocol::CancelProvenance {
    db.get_execution(execution_id)
        .unwrap()
        .cancel_provenance
        .expect("decision-terminal execution must carry cancel provenance")
}

fn seed_ready_chore(db: &WorkDb, label: &str) -> (Task, WorkExecution) {
    let product = create_test_product(db);
    let chore = create_test_chore_manual(db, product.id.clone(), label);
    let execution = create_ready_chore_execution(db, chore.id.clone());
    (chore, execution)
}

#[test]
fn cancel_execution_records_the_supplied_provenance() {
    let db = WorkDb::open(temp_db_path("cancel-prov-rpc")).unwrap();
    let (_, execution) = seed_ready_chore(&db, "rpc cancel");
    let provenance =
        boss_protocol::CancelProvenance::new(CancelSource::Bossctl, "operator", "wrong repo; stopping this run")
            .unwrap();

    let cancelled = db.cancel_execution(&execution.id, provenance.clone()).unwrap();
    assert_eq!(cancelled.status, ExecutionStatus::Cancelled);
    assert_eq!(cancelled.cancel_provenance.as_ref(), Some(&provenance));
    assert_eq!(provenance_of(&db, &execution.id), provenance);
}

#[test]
fn cancel_running_execution_records_kanban_backlog_provenance() {
    let db = WorkDb::open(temp_db_path("cancel-prov-kanban")).unwrap();
    let (chore, execution) = seed_ready_chore(&db, "kanban cancel");
    db.start_execution_run(
        &execution.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
    let provenance = boss_protocol::CancelProvenance::engine(
        CancelSource::WorkItemMovedToBacklog,
        "session abc",
        format!(
            "work item {} moved from active back to todo (kanban drag to Backlog)",
            chore.id
        ),
    );

    assert!(db.cancel_running_execution(&execution.id, provenance.clone()).unwrap());
    assert_eq!(provenance_of(&db, &execution.id), provenance);
}

#[test]
fn mark_execution_redundant_records_double_spawn_guard_provenance() {
    let db = WorkDb::open(temp_db_path("cancel-prov-redundant")).unwrap();
    let (_, execution) = seed_ready_chore(&db, "redundant");
    let provenance = boss_protocol::CancelProvenance::engine(
        CancelSource::DoubleSpawnGuard,
        "execution_coordinator",
        format!(
            "redundant duplicate: live execution other already holds work item {}",
            execution.work_item_id
        ),
    );

    db.mark_execution_redundant(&execution.id, provenance.clone()).unwrap();
    let after = db.get_execution(&execution.id).unwrap();
    assert_eq!(after.status, ExecutionStatus::Abandoned);
    assert_eq!(after.cancel_provenance.as_ref(), Some(&provenance));
}

#[test]
fn blocking_a_running_dependent_records_dependency_block_provenance() {
    let db = WorkDb::open(temp_db_path("cancel-prov-block")).unwrap();
    let product = create_test_product(&db);
    let prereq = create_test_chore_manual(&db, product.id.clone(), "Prereq");
    let dependent = create_test_chore(&db, product.id.clone(), "Running dependent");
    let execution = create_ready_chore_execution(&db, dependent.id.clone());
    db.start_execution_run(
        &execution.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-006",
        "/tmp/mono-agent-006",
    )
    .unwrap();

    let (_, reaped) = db
        .add_dependency_with_worker_reconcile(AddDependencyInput {
            dependent: dependent.id.clone(),
            prerequisite: prereq.id.clone(),
            relation: None,
        })
        .unwrap();
    let reaped = reaped.expect("blocking a running dependent must cancel its execution");
    let provenance = reaped.cancel_provenance.expect("block-cancel stamps provenance");
    assert_eq!(provenance.source, CancelSource::DependencyBlock);
    assert_eq!(provenance.actor, "work_dependencies");
    assert!(
        provenance.reason.contains(&prereq.id),
        "reason must name the gating prereq, got {}",
        provenance.reason
    );
    assert_eq!(provenance_of(&db, &execution.id), provenance);
}

#[test]
fn heal_ghost_active_records_ghost_active_heal_provenance() {
    let db = WorkDb::open(temp_db_path("cancel-prov-ghost")).unwrap();
    let (chore, execution) = seed_ready_chore(&db, "ghost active");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let healed = db.heal_ghost_active_chores().unwrap();
    assert!(
        healed.iter().any(|row| row.work_item_id == chore.id),
        "ghost-active heal must demote the active card with no run history"
    );
    let after = db.get_execution(&execution.id).unwrap();
    assert_eq!(after.status, ExecutionStatus::Abandoned);
    let provenance = after.cancel_provenance.expect("heal stamps provenance");
    assert_eq!(provenance.source, CancelSource::GhostActiveHeal);
    assert_eq!(provenance.actor, "heal_ghost_active_chores");
    assert!(provenance.reason.contains(&chore.id));
}

#[test]
fn stranded_sweep_records_stranded_execution_sweep_provenance() {
    let db = WorkDb::open(temp_db_path("cancel-prov-stranded")).unwrap();
    let (chore, execution) = seed_ready_chore(&db, "stranded");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let abandoned = db.abandon_stranded_executions_on_closed_work_items().unwrap();
    assert!(
        abandoned.iter().any(|row| row.execution_id == execution.id),
        "stranded sweep must abandon the never-started execution on a done item"
    );
    let provenance = provenance_of(&db, &execution.id);
    assert_eq!(provenance.source, CancelSource::StrandedExecutionSweep);
    assert_eq!(provenance.actor, "abandon_stranded_executions_on_closed_work_items");
    assert!(provenance.reason.contains(&chore.id));
}

#[test]
fn request_execution_stale_redispatch_records_stale_governing_provenance() {
    let db = WorkDb::open(temp_db_path("cancel-prov-stale-gov")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Stale governing");
    let stale = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::WaitingHuman)
                .build(),
        )
        .unwrap();

    db.request_execution_with_live_check(
        RequestExecutionInput::builder().work_item_id(chore.id.clone()).build(),
        |_| false,
    )
    .unwrap();

    let provenance = provenance_of(&db, &stale.id);
    assert_eq!(db.get_execution(&stale.id).unwrap().status, ExecutionStatus::Abandoned);
    assert_eq!(provenance.source, CancelSource::StaleGoverningExecution);
    assert_eq!(provenance.actor, "request_execution");
    assert!(provenance.reason.contains(&stale.id));
}

#[test]
fn retire_stale_revision_records_stale_revision_retirement_provenance() {
    let db = WorkDb::open(temp_db_path("cancel-prov-retire")).unwrap();
    let product = make_revision_product(&db, "cancel-prov-retire");
    let root = make_chore_root(&db, &product, "cancel-prov-retire");
    let revision = insert_revision_row(&db, &product, &root);
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let provenance = boss_protocol::CancelProvenance::engine(
        CancelSource::StaleRevisionRetirement,
        "execution_coordinator",
        "merge-conflict revision's bound PR was already resolved by attempt crz_1 before dispatch; no worker is needed",
    );

    assert!(
        db.retire_stale_revision_before_dispatch(&exec.id, &revision, provenance.clone())
            .unwrap()
    );
    assert_eq!(provenance_of(&db, &exec.id), provenance);
}

#[test]
fn deferring_a_work_item_records_work_item_settled_provenance() {
    let db = WorkDb::open(temp_db_path("cancel-prov-deferred")).unwrap();
    let (chore, execution) = seed_ready_chore(&db, "deferred");

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            deferred: Some(true),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let after = db.get_execution(&execution.id).unwrap();
    assert_eq!(after.status, ExecutionStatus::Abandoned);
    let provenance = after.cancel_provenance.expect("defer stamps provenance");
    assert_eq!(provenance.source, CancelSource::WorkItemSettled);
    assert_eq!(provenance.actor, "abandon_pending_executions");
    assert!(provenance.reason.contains("deferred"));
}

#[test]
fn decision_terminal_update_without_provenance_is_aborted() {
    let db = WorkDb::open(temp_db_path("cancel-prov-trigger")).unwrap();
    let (_, execution) = seed_ready_chore(&db, "trigger");
    {
        let conn = db.connect().unwrap();

        let cancelled = conn.execute(
            "UPDATE work_executions SET status = 'cancelled' WHERE id = ?1",
            [&execution.id],
        );
        let cancelled_err = cancelled.expect_err("cancel without provenance must abort");
        assert!(
            cancelled_err.to_string().contains("requires cancel_source"),
            "got {cancelled_err}"
        );

        let abandoned = conn.execute(
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            [&execution.id],
        );
        let abandoned_err = abandoned.expect_err("abandon without provenance must abort");
        assert!(
            abandoned_err.to_string().contains("requires cancel_source"),
            "got {abandoned_err}"
        );
    }

    assert_eq!(
        db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Ready,
        "the aborted writes must leave the row non-terminal"
    );
}
