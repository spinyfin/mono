use super::*;
use crate::test_support::{
    create_ready_chore_execution, create_test_chore, create_test_chore_manual, create_test_product, open_db,
};
use boss_protocol::{CreateExecutionInput, ExecutionStatus};

#[test]
fn legacy_split_opens_agy_at_its_configured_share() {
    let (_dir, db) = open_db();
    db.set_metadata(
        METADATA_KEY_DRIVER_TRAFFIC_SPLIT,
        r#"{"grok":35,"claude":10,"codex":20,"agy":35}"#,
    )
    .unwrap();
    let split = db.get_driver_traffic_split().unwrap();
    assert!(split.launch_enabled.agy);
    let eligible = eligible_drivers_for(&TaskKind::Chore, &ExecutionKind::ChoreImplementation, None, false);
    let launch_eligible = filter_launch_enabled(&split, &eligible);
    let mut counts = std::collections::BTreeMap::new();
    for i in 0..10_000 {
        let driver = allocate_among(split, &format!("launch-sample-{i}"), &TaskKind::Chore, &launch_eligible).unwrap();
        *counts.entry(driver).or_insert(0) += 1;
    }
    assert!((3300..3700).contains(&counts["agy"]), "{counts:?}");
    let product = create_test_product(&db);
    let actual = super::tests::allocated_drivers(&db, &product.id, 200);
    assert!(actual.iter().any(|driver| driver.as_deref() == Some("agy")));
}

#[test]
fn launch_flags_persist_and_revalidate_queued_allocations_with_auditable_exclusions() {
    let (dir, db) = open_db();
    let mut split = DriverTrafficSplit::new(0, 0, 0, 100);
    db.set_driver_traffic_split(split).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "launch gate");
    let execution = create_ready_chore_execution(&db, &chore.id);
    assert_eq!(
        db.get_execution_driver_decision(&execution.id)
            .unwrap()
            .unwrap()
            .driver
            .as_deref(),
        Some("agy")
    );

    split = DriverTrafficSplit::new(35, 10, 20, 35);
    split.launch_enabled.agy = false;
    db.set_driver_traffic_split(split).unwrap();
    drop(db);
    let db = WorkDb::open_existing(dir.path().join("state.db")).unwrap();
    assert_eq!(db.get_driver_traffic_split().unwrap(), split);
    assert!(matches!(
        db.claim_execution_for_dispatch(&execution.id).unwrap(),
        DispatchClaimOutcome::Won
    ));
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_ne!(decision.driver.as_deref(), Some("agy"));
    let snapshot: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT split_at_decision FROM execution_driver_decisions WHERE execution_id=?1",
            [&execution.id],
            |row| row.get(0),
        )
        .unwrap();
    let snapshot: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(
        snapshot["exclusions"],
        serde_json::json!(["agy excluded: launch flag off"])
    );
    assert_eq!(snapshot["launch_enabled"]["agy"], false);
    let actual = super::tests::allocated_drivers(&db, &product.id, 200);
    assert!(actual.iter().all(|driver| driver.as_deref() != Some("agy")));

    split.launch_enabled.agy = true;
    db.set_driver_traffic_split(split).unwrap();
    drop(db);
    let db = WorkDb::open_existing(dir.path().join("state.db")).unwrap();
    assert!(db.get_driver_traffic_split().unwrap().launch_enabled.agy);
}

#[test]
fn closed_flag_preserves_explicit_canaries_and_reports_all_disabled_failure() {
    let (_dir, db) = open_db();
    let mut split = DriverTrafficSplit::new(0, 0, 0, 100);
    split.launch_enabled.agy = false;
    db.set_driver_traffic_split(split).unwrap();
    let product = create_test_product(&db);
    let chore = crate::test_support::create_test_chore_manual(&db, &product.id, "explicit canary");
    let err =
        decide_execution_driver(&db.connect().unwrap(), &chore.id, ExecutionKind::ChoreImplementation).unwrap_err();
    assert!(err.to_string().contains("agy excluded: launch flag off"));
    let refused = err.downcast_ref::<DriverAllocationRefused>().unwrap();
    assert_eq!(refused.split_at_decision, Some(split));
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET driver='agy' WHERE id=?1", [&chore.id])
        .unwrap();
    let decision =
        decide_execution_driver(&db.connect().unwrap(), &chore.id, ExecutionKind::ChoreImplementation).unwrap();
    assert_eq!(decision.reason, REASON_EXPLICIT);
    assert_eq!(decision.driver.as_deref(), Some("agy"));
}

fn decision_snapshot(db: &WorkDb, execution_id: &str) -> serde_json::Value {
    let raw: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT split_at_decision FROM execution_driver_decisions WHERE execution_id=?1",
            [execution_id],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[test]
fn insertion_records_launch_exclusions_when_the_only_funded_driver_is_closed() {
    let (_dir, db) = open_db();
    let mut split = DriverTrafficSplit::new(0, 0, 0, 100);
    split.launch_enabled.agy = false;
    db.set_driver_traffic_split(split).unwrap();
    let product = create_test_product(&db);
    let chore = crate::test_support::create_test_chore_manual(&db, &product.id, "agy closed");
    let execution = create_ready_chore_execution(&db, &chore.id);
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.reason, REASON_ALLOCATION_FAILED);
    assert_eq!(decision.driver, None);
    assert_eq!(decision.split_at_decision, Some(split));
    let snapshot = decision_snapshot(&db, &execution.id);
    assert_eq!(
        snapshot["exclusions"],
        serde_json::json!(["agy excluded: launch flag off"])
    );
    assert_eq!(snapshot["launch_enabled"]["agy"], false);
}

#[test]
fn claim_records_a_failed_decision_when_a_later_flag_change_makes_allocation_impossible() {
    let (_dir, db) = open_db();
    let mut split = DriverTrafficSplit::new(0, 0, 0, 100);
    db.set_driver_traffic_split(split).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "queued then closed");
    let execution = create_ready_chore_execution(&db, &chore.id);
    assert_eq!(
        db.get_execution_driver_decision(&execution.id)
            .unwrap()
            .unwrap()
            .driver
            .as_deref(),
        Some("agy")
    );
    split.launch_enabled.agy = false;
    db.set_driver_traffic_split(split).unwrap();
    let err = db.claim_execution_for_dispatch(&execution.id).unwrap_err();
    assert!(err.downcast_ref::<DriverAllocationRefused>().is_some(), "{err}");
    assert!(err.to_string().contains("agy excluded: launch flag off"), "{err}");
    assert_eq!(db.get_execution(&execution.id).unwrap().status, ExecutionStatus::Ready);
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.reason, REASON_ALLOCATION_FAILED);
    assert_eq!(decision.driver, None);
    assert_eq!(decision.split_at_decision, Some(split));
    let snapshot = decision_snapshot(&db, &execution.id);
    assert_eq!(
        snapshot["exclusions"],
        serde_json::json!(["agy excluded: launch flag off"])
    );
}

#[test]
fn launch_flag_and_share_writers_do_not_clobber_each_other() {
    let (_dir, db) = open_db();
    let mut stale = DriverTrafficSplit::new(35, 10, 20, 35);
    db.set_driver_traffic_split(stale).unwrap();
    db.set_driver_launch_flag("agy", false).unwrap();
    stale.grok = 40;
    stale.claude = 5;
    stale.launch_enabled.agy = true;
    let applied = db.set_driver_traffic_shares(stale).unwrap();
    assert!(!applied.launch_enabled.agy);
    assert_eq!(applied.grok, 40);
    assert_eq!(applied.claude, 5);
    db.set_driver_launch_flag("claude", false).unwrap();
    db.set_driver_launch_flag("agy", true).unwrap();
    let stored = db.get_driver_traffic_split().unwrap();
    assert!(stored.launch_enabled.agy);
    assert!(!stored.launch_enabled.claude);
    assert!(stored.launch_enabled.codex);
    assert_eq!(stored.grok, 40);
}

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
fn failed_reallocation_rolls_back_the_claim_and_records_the_failed_decision() {
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
    let split = DriverTrafficSplit::new(100, 0, 0, 0);
    db.set_driver_traffic_split(split).unwrap();
    let err = db.claim_execution_for_dispatch(&execution.id).unwrap_err();
    assert!(err.to_string().contains("0 share"), "{err}");
    assert_eq!(db.get_execution(&execution.id).unwrap().status, ExecutionStatus::Ready);
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.reason, REASON_ALLOCATION_FAILED);
    assert_eq!(decision.driver, None);
    assert_eq!(decision.split_at_decision, Some(split));
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
    let pinned = create_test_chore(&db, &product.id, "pinned to an unfunded driver's model");
    db.update_work_item(
        &pinned.id,
        boss_protocol::WorkItemPatch {
            model_override: Some("opus".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let other = create_test_chore(&db, &product.id, "unpinned");

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

#[test]
fn work_resumed_attention_survives_a_later_pre_start_failure() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, &product.id, "terminal error then refusal");
    let first = create_ready_chore_execution(&db, &chore.id);
    let second = create_ready_chore_execution(&db, &chore.id);
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_attention_items(id,execution_id,kind,status,title,body_markdown,created_at)
             VALUES ('att1',?1,'driver_terminal_error','open','fixture','','1')",
            rusqlite::params![first.id],
        )
        .unwrap();
    db.record_pre_start_failure(&second.id, "engine", None, "refused", &[])
        .unwrap();
    db.reconcile_stale_attention_signals().unwrap();
    let status: String = db
        .connect()
        .unwrap()
        .query_row("SELECT status FROM work_attention_items WHERE id='att1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(status, "open", "a failed bookkeeping run is not a resumed run");
}
