use super::*;
use crate::test_support::{create_ready_chore_execution, create_test_chore_manual, create_test_product, open_db};
use boss_protocol::{CreateExecutionInput, ExecutionStatus};

#[test]
fn dispatch_claim_reallocates_a_driver_disabled_while_queued_even_after_restart() {
    let (dir, db) = open_db();
    let product = create_test_product(&db);
    db.set_driver_traffic_split(DriverTrafficSplit::new(100, 0, 0, 0))
        .unwrap();
    let chore = create_test_chore_manual(&db, &product.id, "queued before share change");
    let execution = create_ready_chore_execution(&db, &chore.id);
    assert_eq!(
        db.get_execution_driver_decision(&execution.id)
            .unwrap()
            .unwrap()
            .driver
            .as_deref(),
        Some("grok")
    );
    let current = DriverTrafficSplit::new(0, 50, 50, 0);
    db.set_driver_traffic_split(current).unwrap();
    drop(db);
    let db = WorkDb::open_existing(dir.path().join("state.db")).unwrap();
    assert_eq!(db.get_driver_traffic_split().unwrap(), current);
    assert!(matches!(
        db.claim_execution_for_dispatch(&execution.id).unwrap(),
        DispatchClaimOutcome::Won
    ));
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_ne!(decision.driver.as_deref(), Some("grok"));
    assert_eq!(decision.reason, REASON_ALLOCATION);
    assert_eq!(decision.split_at_decision, Some(current));
}

#[test]
fn every_dispatch_kind_respects_zero_share_or_its_explicit_pool_policy() {
    for kind in [
        ExecutionKind::TaskImplementation,
        ExecutionKind::ChoreImplementation,
        ExecutionKind::RevisionImplementation,
        ExecutionKind::PrReview,
    ] {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        db.set_driver_traffic_split(DriverTrafficSplit::new(100, 0, 0, 0))
            .unwrap();
        let chore = create_test_chore_manual(&db, &product.id, "queued dispatch");
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&chore.id)
                    .kind(kind.clone())
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100, 0))
            .unwrap();
        assert!(matches!(
            db.claim_execution_for_dispatch(&execution.id).unwrap(),
            DispatchClaimOutcome::Won
        ));
        let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        if kind == ExecutionKind::PrReview {
            assert_eq!(decision.reason, REASON_POOL);
            assert_eq!(decision.driver.as_deref(), Some("claude"));
        } else {
            assert_eq!(decision.reason, REASON_ALLOCATION);
            assert_eq!(decision.driver.as_deref(), Some("codex"));
        }
    }
}

#[test]
fn failed_reallocation_rolls_back_the_claim_and_preserves_the_decision() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, &product.id, "disabled eligible driver");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&chore.id)
                .kind(ExecutionKind::ConflictResolution)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let previous = db.get_execution_driver_decision(&execution.id).unwrap();
    db.set_driver_traffic_split(DriverTrafficSplit::new(100, 0, 0, 0))
        .unwrap();
    let err = db.claim_execution_for_dispatch(&execution.id).unwrap_err();
    assert!(err.to_string().contains("0 share"), "{err}");
    assert_eq!(db.get_execution(&execution.id).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(db.get_execution_driver_decision(&execution.id).unwrap(), previous);
}

#[test]
fn revalidation_covers_legacy_and_missing_decisions_without_reassigning_live_reads() {
    for reason in [Some(REASON_LEGACY_PERCENTAGE), Some(REASON_DEFAULT), None] {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        db.set_driver_traffic_split(DriverTrafficSplit::new(100, 0, 0, 0))
            .unwrap();
        let chore = create_test_chore_manual(&db, &product.id, "old execution");
        let execution = create_ready_chore_execution(&db, &chore.id);
        {
            let conn = db.connect().unwrap();
            if let Some(reason) = reason {
                conn.execute(
                    "UPDATE execution_driver_decisions SET reason = ?1 WHERE execution_id = ?2",
                    params![reason, execution.id],
                )
                .unwrap();
            } else {
                conn.execute(
                    "DELETE FROM execution_driver_decisions WHERE execution_id = ?1",
                    [&execution.id],
                )
                .unwrap();
            }
        }
        let previous = db.get_execution_driver_decision(&execution.id).unwrap();
        db.set_driver_traffic_split(DriverTrafficSplit::new(0, 100, 0, 0))
            .unwrap();
        assert_eq!(db.get_execution_driver_decision(&execution.id).unwrap(), previous);
        let refreshed = db.refresh_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(refreshed.reason, REASON_ALLOCATION);
        assert_eq!(refreshed.driver.as_deref(), Some("claude"));
    }
}

#[test]
fn an_unallocatable_task_does_not_stop_its_products_other_executions() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100, 0))
        .unwrap();
    // A model pin is an eligibility constraint, not a driver pin: "opus"
    // belongs to claude, which holds no share, so this row cannot be placed.
    let pinned = crate::test_support::create_test_chore(&db, &product.id, "pinned to an unfunded driver's model");
    db.update_work_item(
        &pinned.id,
        boss_protocol::WorkItemPatch {
            model_override: Some("opus".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let other = crate::test_support::create_test_chore(&db, &product.id, "unpinned");

    let result = db.reconcile_product_executions(&product.id).unwrap();
    assert!(result.created.iter().any(|e| e.work_item_id == other.id));
    let refused = result
        .created
        .iter()
        .find(|e| e.work_item_id == pinned.id)
        .expect("the offending row still gets an execution");
    let decision = db.get_execution_driver_decision(&refused.id).unwrap().unwrap();
    assert_eq!(decision.reason, REASON_ALLOCATION_FAILED);
    assert_eq!(decision.driver, None);
    let other_decision = db
        .get_execution_driver_decision(&result.created.iter().find(|e| e.work_item_id == other.id).unwrap().id)
        .unwrap()
        .unwrap();
    assert_eq!(other_decision.driver.as_deref(), Some("codex"));

    // Claim revalidation refuses only the offending execution, with a typed error.
    let err = db.claim_execution_for_dispatch(&refused.id).unwrap_err();
    let refusal = err.downcast_ref::<DriverAllocationRefused>().expect("typed refusal");
    assert!(!refusal.to_string().is_empty());
}

#[test]
fn attention_sweep_keeps_the_banner_of_a_pre_start_failure_until_a_real_run_starts() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, &product.id, "refused at dispatch");
    let execution = create_ready_chore_execution(&db, &chore.id);

    // A terminal pre-start failure writes a failed bookkeeping run stamped
    // `started_at = now` in the same second as the bounce's banner stamp.
    db.record_pre_start_failure(&execution.id, "engine", None, "refused", &[])
        .unwrap();
    db.bounce_dispatch_failed_to_backlog(&chore.id, "driver_allocation_refused", "refused")
        .unwrap();
    let banner = |db: &WorkDb| -> Option<String> {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT dispatch_failed_reason FROM tasks WHERE id = ?1",
                [&chore.id],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert!(banner(&db).is_some());
    db.reconcile_stale_attention_signals().unwrap();
    assert_eq!(
        banner(&db).as_deref(),
        Some("driver_allocation_refused"),
        "a failed bookkeeping run is not evidence that anything started"
    );

    // A genuine later run start does clear it.
    db.connect()
        .unwrap()
        .execute(
        "INSERT INTO work_runs (id, execution_id, agent_id, status, created_at, started_at)
         VALUES ('run_real', ?1, 'worker', 'running', '1', CAST((SELECT CAST(dispatch_failed_at AS INTEGER) + 5 FROM tasks WHERE id = ?2) AS TEXT))",
        rusqlite::params![execution.id, chore.id],
    )
    .unwrap();
    db.reconcile_stale_attention_signals().unwrap();
    assert_eq!(banner(&db), None);
}
