//! Tests for three-way driver traffic allocation.
//!
//! The end-to-end cases deliberately go through `create_ready_chore_execution`
//! (which funnels into `insert_execution`) rather than calling
//! `decide_execution_driver` directly, so they exercise the same path real
//! dispatch takes: decide once at insert, persist, read back.

use super::*;
use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
use boss_protocol::{DRIVER_SLUG_CLAUDE, DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK, WorkItemPatch};

/// Every driver an eligible row was allocated to across `count` fresh chores.
fn allocated_drivers(db: &WorkDb, product_id: &str, count: usize) -> Vec<Option<String>> {
    (0..count)
        .map(|i| {
            let chore = create_test_chore(db, product_id, format!("chore {i}"));
            let execution = create_ready_chore_execution(db, &chore.id);
            let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
            assert_eq!(decision.reason, REASON_ALLOCATION, "eligible row must be allocated");
            decision.driver
        })
        .collect()
}

#[test]
fn hash_bucket_is_stable_across_calls() {
    let id = "task_abc123";
    assert_eq!(hash_bucket(id), hash_bucket(id));
}

#[test]
fn hash_bucket_is_in_range() {
    for id in ["task_a", "task_b", "task_c", "exec_zzz", ""] {
        assert!(hash_bucket(id) < 100, "bucket out of range for {id:?}");
    }
}

/// The shipped default is behaviour-preserving: never configured means
/// everything allocates to the engine default driver, and `grok` — whose
/// spawn path is currently broken — gets nothing.
#[test]
fn split_defaults_to_all_claude_and_persists_across_get() {
    let (_dir, db) = open_db();
    let default = db.get_driver_traffic_split().unwrap();
    assert_eq!(default, DriverTrafficSplit::new(0, 100, 0));
    assert_eq!(default.grok, 0, "grok must ship at 0");
    assert_eq!(
        boss_engine_effort::ENGINE_DEFAULT_DRIVER,
        DRIVER_SLUG_CLAUDE,
        "the default split's 100% driver must be the engine default, or the default is not behaviour-preserving",
    );

    let split = DriverTrafficSplit::new(20, 50, 30);
    assert_eq!(db.set_driver_traffic_split(split).unwrap(), split);
    assert_eq!(db.get_driver_traffic_split().unwrap(), split);
}

/// An invalid split is refused outright — not clamped, redistributed, or
/// normalised — and leaves the persisted value untouched.
#[test]
fn set_rejects_splits_that_do_not_sum_to_one_hundred() {
    let (_dir, db) = open_db();
    let good = DriverTrafficSplit::new(10, 60, 30);
    db.set_driver_traffic_split(good).unwrap();

    for bad in [
        DriverTrafficSplit::new(0, 0, 0),
        DriverTrafficSplit::new(50, 50, 50),
        DriverTrafficSplit::new(10, 60, 10),
        DriverTrafficSplit::new(0, 99, 0),
    ] {
        let err = db.set_driver_traffic_split(bad).unwrap_err();
        assert!(err.to_string().contains("sum to exactly 100"), "{err}");
        assert_eq!(
            db.get_driver_traffic_split().unwrap(),
            good,
            "a rejected split must not have been written",
        );
    }
}

/// A hand-edited `state.db` holding garbage falls back to the default split
/// rather than wedging every execution insert — and never to a repaired
/// version of the garbage.
#[test]
fn unparseable_or_invalid_persisted_split_falls_back_to_the_default() {
    for raw in [r#"{"grok":50,"claude":50,"codex":50}"#, "not json", r#"{"grok":0}"#] {
        let (_dir, db) = open_db();
        db.set_metadata(METADATA_KEY_DRIVER_TRAFFIC_SPLIT, raw).unwrap();
        assert_eq!(
            db.get_driver_traffic_split().unwrap(),
            DriverTrafficSplit::default(),
            "raw {raw:?}",
        );
    }
}

#[test]
fn is_allocation_eligible_whitelists_implementation_kinds_only() {
    assert!(is_allocation_eligible(ExecutionKind::TaskImplementation));
    assert!(is_allocation_eligible(ExecutionKind::ChoreImplementation));
    assert!(is_allocation_eligible(ExecutionKind::RevisionImplementation));
    assert!(!is_allocation_eligible(ExecutionKind::ProductDesign));
    assert!(!is_allocation_eligible(ExecutionKind::ProjectDesign));
    assert!(!is_allocation_eligible(ExecutionKind::InvestigationImplementation));
    assert!(!is_allocation_eligible(ExecutionKind::PrReview));
    assert!(!is_allocation_eligible(ExecutionKind::AutomationTriage));
    assert!(!is_allocation_eligible(ExecutionKind::CiRemediation));
    assert!(!is_allocation_eligible(ExecutionKind::ConflictResolution));
    assert!(!is_allocation_eligible(ExecutionKind::AnswerAgent));
}

/// End-to-end: a driver at 0 receives literally zero eligible rows, across a
/// wide spread of work item ids (not a hand-picked one that happens to hash
/// into a convenient bucket). Run for each driver in turn, so "zero is zero"
/// is proven for all three, not just the one that used to be special.
#[test]
fn a_driver_at_zero_receives_no_rows_end_to_end() {
    for (split, zeroed) in [
        (DriverTrafficSplit::new(0, 50, 50), DRIVER_SLUG_GROK),
        (DriverTrafficSplit::new(50, 0, 50), DRIVER_SLUG_CLAUDE),
        (DriverTrafficSplit::new(50, 50, 0), DRIVER_SLUG_CODEX),
    ] {
        let (_dir, db) = open_db();
        db.set_driver_traffic_split(split).unwrap();
        let product = create_test_product(&db);
        for driver in allocated_drivers(&db, &product.id, 200) {
            assert_ne!(
                driver.as_deref(),
                Some(zeroed),
                "{zeroed} is at 0 under {split:?} and must receive nothing",
            );
        }
    }
}

/// Two of three at zero: every eligible row goes to the survivor.
#[test]
fn two_drivers_at_zero_sends_everything_to_the_third() {
    for (split, expected) in [
        (DriverTrafficSplit::new(100, 0, 0), DRIVER_SLUG_GROK),
        (DriverTrafficSplit::new(0, 100, 0), DRIVER_SLUG_CLAUDE),
        (DriverTrafficSplit::new(0, 0, 100), DRIVER_SLUG_CODEX),
    ] {
        let (_dir, db) = open_db();
        db.set_driver_traffic_split(split).unwrap();
        let product = create_test_product(&db);
        for driver in allocated_drivers(&db, &product.id, 50) {
            assert_eq!(driver.as_deref(), Some(expected), "under {split:?}");
        }
    }
}

/// The decision records the split it was measured against, so the experiment
/// can actually be evaluated after the fact.
#[test]
fn allocation_records_the_split_it_was_decided_under() {
    let (_dir, db) = open_db();
    let split = DriverTrafficSplit::new(25, 50, 25);
    db.set_driver_traffic_split(split).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "recorded chore");
    let execution = create_ready_chore_execution(&db, &chore.id);
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.split_at_decision, Some(split));
    assert_eq!(decision.reason, REASON_ALLOCATION);
    assert!(decision.driver.is_some(), "allocation always names a driver");
}

/// An explicit `--driver` on the row always wins, regardless of the split.
#[test]
fn explicit_row_driver_overrides_allocation() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "explicit-driver chore");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            driver: Some("copilot".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let execution = create_ready_chore_execution(&db, &chore.id);
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.driver.as_deref(), Some("copilot"));
    assert_eq!(decision.reason, REASON_EXPLICIT);
    assert_eq!(decision.split_at_decision, None);
}

/// A product that pinned a `default_driver` has expressed a preference, so
/// allocation does not decide its rows. This is what makes the shipped
/// default byte-for-byte behaviour-preserving for such a product, rather
/// than silently dragging it onto `claude`.
#[test]
fn product_default_driver_overrides_allocation() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);
    db.update_work_item(
        &product.id,
        WorkItemPatch {
            default_driver: Some("copilot".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let chore = create_test_chore(&db, &product.id, "product-pinned chore");
    let execution = create_ready_chore_execution(&db, &chore.id);
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.driver.as_deref(), Some("copilot"));
    assert_eq!(decision.reason, REASON_EXPLICIT);
}

/// A row whose reasoning is not `standard` (e.g. `investigation`) is never
/// eligible, whatever the split says.
#[test]
fn non_standard_reasoning_is_ineligible() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "investigation-shaped chore");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            reasoning: Some("investigation".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let execution = create_ready_chore_execution(&db, &chore.id);
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.driver, None);
    assert_eq!(decision.reason, REASON_DEFAULT);
}

/// Same work item id → same decision, deterministically, across repeated
/// (re-)dispatch of independent execution rows — the whole point of hashing
/// the id instead of rolling per attempt.
#[test]
fn same_work_item_id_is_stable_across_repeated_dispatch() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(33, 33, 34))
        .unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "retried chore");
    let first = create_ready_chore_execution(&db, &chore.id);
    let second = create_ready_chore_execution(&db, &chore.id);
    let d1 = db.get_execution_driver_decision(&first.id).unwrap().unwrap();
    let d2 = db.get_execution_driver_decision(&second.id).unwrap().unwrap();
    assert_eq!(
        d1.driver, d2.driver,
        "same work item id must resolve to the same driver every time",
    );
}

/// Changing the split does not rewrite a decision already recorded. A row
/// cannot flip driver between attempts of the same dispatch, and work
/// already dispatched is never reassigned.
#[test]
fn changing_the_split_does_not_rewrite_an_existing_decision() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "already-decided chore");
    let execution = create_ready_chore_execution(&db, &chore.id);
    let before = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(before.driver.as_deref(), Some(DRIVER_SLUG_CODEX));

    db.set_driver_traffic_split(DriverTrafficSplit::new(100, 0, 0)).unwrap();
    let after = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(after, before, "an existing decision must never be recomputed");
}

/// A driver pinned on the row AFTER its execution already exists still wins.
/// The allocation is frozen at insert time, but the pin is read live —
/// "an explicit driver on the row always wins" is about the row, not about
/// the instant the execution was created.
#[test]
fn a_pin_applied_after_the_execution_exists_still_wins() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "late-pin chore");
    let execution = create_ready_chore_execution(&db, &chore.id);
    assert_eq!(
        db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
        Some(DRIVER_SLUG_CODEX),
        "unpinned row must route to its allocation",
    );

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            driver: Some("copilot".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
        Some("copilot"),
        "a pin applied after the execution row exists must still win over the allocation",
    );

    // And removing it again falls back to the allocation, not to a stale
    // recorded pin.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            driver: Some(String::new()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
        Some(DRIVER_SLUG_CODEX),
    );
}

/// A product pin applied after the fact wins over the allocation too, and
/// the row's own pin still outranks it.
#[test]
fn a_product_pin_applied_after_the_execution_exists_still_wins() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "late-product-pin chore");
    let execution = create_ready_chore_execution(&db, &chore.id);

    db.update_work_item(
        &product.id,
        WorkItemPatch {
            default_driver: Some("copilot".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
        Some("copilot"),
    );

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            driver: Some("cursor".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
        Some("cursor"),
        "the row's own pin outranks the product pin",
    );
}

/// An allocated row with no pin anywhere actually routes to the driver the
/// split chose — the recorded decision is not an inert audit trail.
#[test]
fn an_allocated_row_routes_to_its_allocated_driver() {
    for (split, expected) in [
        (DriverTrafficSplit::new(100, 0, 0), DRIVER_SLUG_GROK),
        (DriverTrafficSplit::new(0, 0, 100), DRIVER_SLUG_CODEX),
        (DriverTrafficSplit::new(0, 100, 0), DRIVER_SLUG_CLAUDE),
    ] {
        let (_dir, db) = open_db();
        db.set_driver_traffic_split(split).unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "routed chore");
        let execution = create_ready_chore_execution(&db, &chore.id);
        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
            Some(expected),
            "under {split:?}",
        );
    }
}

/// A decision row written by the superseded percentage scheme still reads
/// back with its own reason rather than degrading to `default`, and its
/// driver still routes.
#[test]
fn legacy_percentage_decision_rows_still_read_back() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "legacy chore");
    let execution = create_ready_chore_execution(&db, &chore.id);
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE execution_driver_decisions SET driver = ?1, reason = ?2, split_at_decision = NULL WHERE execution_id = ?3",
        params![DRIVER_SLUG_CODEX, REASON_LEGACY_PERCENTAGE, &execution.id],
    )
    .unwrap();
    drop(conn);

    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.reason, REASON_LEGACY_PERCENTAGE);
    assert_eq!(decision.driver.as_deref(), Some(DRIVER_SLUG_CODEX));
    assert_eq!(decision.split_at_decision, None);
    assert_eq!(
        db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
        Some(DRIVER_SLUG_CODEX),
        "a legacy decision must still route",
    );
}
