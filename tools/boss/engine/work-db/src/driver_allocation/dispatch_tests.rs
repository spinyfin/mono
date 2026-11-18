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
