use super::*;
use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};

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
    let mut counts = std::collections::BTreeMap::new();
    for i in 0..10_000 {
        let driver = allocate_among(split, &format!("launch-sample-{i}"), &TaskKind::Chore, &eligible).unwrap();
        *counts.entry(driver).or_insert(0) += 1;
    }
    assert!((3300..3700).contains(&counts["agy"]), "{counts:?}");
    println!("10,000 deterministic allocation ids: {counts:?}");
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
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET driver='agy' WHERE id=?1", [&chore.id])
        .unwrap();
    let decision =
        decide_execution_driver(&db.connect().unwrap(), &chore.id, ExecutionKind::ChoreImplementation).unwrap();
    assert_eq!(decision.reason, REASON_EXPLICIT);
    assert_eq!(decision.driver.as_deref(), Some("agy"));
}
