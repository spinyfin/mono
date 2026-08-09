//! Tests for three-way driver traffic allocation.
//!
//! The end-to-end cases deliberately go through `create_ready_chore_execution`
//! (which funnels into `insert_execution`) rather than calling
//! `decide_execution_driver` directly, so they exercise the same path real
//! dispatch takes: decide once at insert, persist, read back.

use super::*;
use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
use boss_protocol::{
    CreateExecutionInput, DRIVER_SLUG_CLAUDE, DRIVER_SLUG_CODEX, DRIVER_SLUG_GROK, ExecutionStatus, WorkItemPatch,
};

/// Rewrite a work item's `kind` in place.
///
/// Allocation reads `tasks.kind` to ask the capability gate what a row of
/// that kind needs, so covering every [`TaskKind`] means having a row of
/// every kind. Writing the column directly (rather than reaching for a
/// per-kind constructor, several of which need a parent/project/doc pointer
/// this test does not care about) keeps the sweep exhaustive over
/// `TaskKind::ALL` — a new variant is covered the day it is added.
fn set_task_kind(db: &WorkDb, work_item_id: &str, kind: &TaskKind) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET kind = ?1 WHERE id = ?2",
        params![kind.as_str(), work_item_id],
    )
    .unwrap();
}

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

/// Eligibility is the dispatch capability gate, not a list this module
/// keeps. Asserted against `check_dispatch` itself for every kind, so a
/// `KindRequirements` or `CapabilitySet` change moves both together or
/// fails here.
///
/// Exercised at `ChoreImplementation` — the execution kind every real
/// allocation site in this file's other tests actually produces
/// (`create_ready_chore_execution`) — since it carries no
/// `ExecutionKind`-level escalation of its own; the escalated kinds
/// (`ConflictResolution` / `CiRemediation`) are covered separately by
/// `conflict_resolution_and_ci_remediation_are_never_allocated`.
#[test]
fn eligibility_is_exactly_what_the_dispatch_gate_says() {
    let registry = crate::driver::DriverRegistry::default();
    let execution_kind = ExecutionKind::ChoreImplementation;
    for kind in TaskKind::ALL {
        let eligible = eligible_drivers_for(kind, &execution_kind, None);
        for driver in DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER {
            let gate_ok = registry
                .resolver(driver)
                .unwrap()
                .check_dispatch(kind, Some(&execution_kind))
                .is_ok();
            assert_eq!(
                eligible.contains(&driver),
                gate_ok,
                "{driver} eligibility for {kind} must be the gate's answer, not a local rule",
            );
        }
    }
}

/// The hard invariant, end to end and across every work-item kind: whatever
/// driver a row is allocated to, the dispatch capability gate accepts that
/// `(kind, driver)` pair — so allocation can never strand a row on a driver
/// that refuses it at spawn.
///
/// This is also the widening this change is about: `design`,
/// `investigation`, `design_postmortem` and friends are allocated now,
/// where the previous hardcoded slice left them on the default driver.
#[test]
fn every_kind_is_allocated_to_a_driver_the_gate_accepts() {
    let registry = crate::driver::DriverRegistry::default();
    for split in [
        DriverTrafficSplit::new(0, 100, 0),
        DriverTrafficSplit::new(33, 33, 34),
        DriverTrafficSplit::new(10, 60, 30),
        DriverTrafficSplit::new(0, 0, 100),
    ] {
        let (_dir, db) = open_db();
        db.set_driver_traffic_split(split).unwrap();
        let product = create_test_product(&db);
        for kind in TaskKind::ALL {
            for i in 0..12 {
                let chore = create_test_chore(&db, &product.id, format!("{kind} row {i}"));
                set_task_kind(&db, &chore.id, kind);
                let execution = create_ready_chore_execution(&db, &chore.id);
                let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
                assert_eq!(decision.reason, REASON_ALLOCATION, "{kind} row must be allocated");
                let driver = decision.driver.expect("an allocated row always names a driver");
                registry
                    .resolver(&driver)
                    .expect("allocation must only ever name a registered driver")
                    .check_dispatch(kind, Some(&ExecutionKind::ChoreImplementation))
                    .unwrap_or_else(|e| {
                        panic!("allocated {driver} for {kind} under {split:?}, but the gate refuses it: {e}")
                    });
            }
        }
    }
}

/// Reasoning mode no longer decides the driver. It still decides the model
/// (`ModelMenu::model_for_reasoning`), which is a per-driver menu lookup at
/// spawn — but an `investigation`-reasoning row, and a legacy row with no
/// reasoning at all, are allocated exactly like a `standard` one.
#[test]
fn reasoning_does_not_gate_allocation() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);

    let investigation_shaped = create_test_chore(&db, &product.id, "investigation-reasoning chore");
    db.update_work_item(
        &investigation_shaped.id,
        WorkItemPatch {
            reasoning: Some("investigation".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();

    let legacy = create_test_chore(&db, &product.id, "legacy chore with no reasoning");
    let conn = db.connect().unwrap();
    conn.execute("UPDATE tasks SET reasoning = NULL WHERE id = ?1", params![&legacy.id])
        .unwrap();
    drop(conn);

    for chore in [&investigation_shaped, &legacy] {
        let execution = create_ready_chore_execution(&db, &chore.id);
        let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(decision.reason, REASON_ALLOCATION, "{} must be allocated", chore.name);
        assert_eq!(decision.driver.as_deref(), Some(DRIVER_SLUG_CODEX));
    }
}

/// A PR review dispatches on the review pool's pinned driver, not on the
/// row's — so its decision must record that actual driver and the pool
/// override, never a row pin. Deliberate: the pool pin is what keeps who
/// authored a change from deciding who reviews it
/// (`coordinator::pool_dispatch_policy_for_worker_id`).
#[test]
fn pr_review_executions_record_the_pool_driver_over_a_row_pin() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "reviewed chore");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            driver: Some(DRIVER_SLUG_CODEX.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let decision = db.get_execution_driver_decision(&review.id).unwrap().unwrap();
    assert_eq!(decision.driver.as_deref(), Some(DRIVER_SLUG_CLAUDE));
    assert_eq!(decision.reason, REASON_POOL);
    assert_eq!(
        db.get_execution_driver_slug(&review.id).unwrap().as_deref(),
        decision.driver.as_deref(),
        "the persisted review decision must name the driver dispatch uses",
    );

    // The implementation execution for the same row still honours its pin —
    // it is the review that is pool-pinned, not the work item.
    let implementation = create_ready_chore_execution(&db, &chore.id);
    let decision = db.get_execution_driver_decision(&implementation.id).unwrap().unwrap();
    assert_eq!(decision.reason, REASON_EXPLICIT);
    assert_eq!(decision.driver.as_deref(), Some(DRIVER_SLUG_CODEX));
}

#[test]
fn pool_driver_backfill_corrects_old_review_decisions_idempotently() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "legacy reviewed chore");
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE execution_driver_decisions
         SET driver = ?1, reason = ?2
         WHERE execution_id = ?3",
        params![DRIVER_SLUG_CODEX, REASON_EXPLICIT, &review.id],
    )
    .unwrap();
    migrate_backfill_pool_driver_decisions(&conn).unwrap();
    migrate_backfill_pool_driver_decisions(&conn).unwrap();
    drop(conn);

    let decision = db.get_execution_driver_decision(&review.id).unwrap().unwrap();
    assert_eq!(decision.driver.as_deref(), Some(DRIVER_SLUG_CLAUDE));
    assert_eq!(decision.reason, REASON_POOL);
}

/// A row whose `work_executions.driver` — the launch tuple that actually
/// reached the spawned worker — names a driver other than the pool driver
/// must be left alone by the backfill: overwriting it would replace a true
/// record of what ran with a false one, which is the exact defect this
/// migration exists to fix, in the opposite direction.
#[test]
fn pool_driver_backfill_leaves_a_contradicting_launch_record_alone() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "legacy reviewed chore on codex");
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE execution_driver_decisions
         SET driver = ?1, reason = ?2
         WHERE execution_id = ?3",
        params![DRIVER_SLUG_CODEX, REASON_EXPLICIT, &review.id],
    )
    .unwrap();
    // The recorded launch tuple says this execution really did run on
    // codex — a pre-pin-fix review, plausibly.
    conn.execute(
        "UPDATE work_executions SET driver = ?1 WHERE id = ?2",
        params![DRIVER_SLUG_CODEX, &review.id],
    )
    .unwrap();
    migrate_backfill_pool_driver_decisions(&conn).unwrap();
    drop(conn);

    let decision = db.get_execution_driver_decision(&review.id).unwrap().unwrap();
    assert_eq!(
        decision.driver.as_deref(),
        Some(DRIVER_SLUG_CODEX),
        "a decision contradicted by its own execution's recorded launch driver must not be overwritten",
    );
    assert_eq!(decision.reason, REASON_EXPLICIT);
}

/// A historical `chore_implementation` execution on an automation-sourced
/// row is pool-bound by the same rule `decide_execution_driver` applies
/// today (`tasks.source_automation_id`), even though its kind is not one of
/// the two pool-bound kinds — the backfill must correct it too.
#[test]
fn pool_driver_backfill_corrects_automation_sourced_ordinary_executions() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "legacy automation-sourced chore");
    let automation = db
        .create_automation(boss_protocol::CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Fix clippy".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "America/Los_Angeles".to_owned(),
            },
            standing_instruction: "Fix any new clippy warnings.".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();
    let execution = create_ready_chore_execution(&db, &chore.id);
    let conn = db.connect().unwrap();
    // Backdate: this row is automation-sourced, but its decision was
    // recorded before pool-bound automation-sourced rows were recognised.
    conn.execute(
        "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
        params![&automation.id, &chore.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE execution_driver_decisions
         SET driver = ?1, reason = ?2
         WHERE execution_id = ?3",
        params![DRIVER_SLUG_CODEX, REASON_EXPLICIT, &execution.id],
    )
    .unwrap();
    migrate_backfill_pool_driver_decisions(&conn).unwrap();
    drop(conn);

    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.driver.as_deref(), Some(DRIVER_SLUG_CLAUDE));
    assert_eq!(decision.reason, REASON_POOL);
}

/// Conflict resolution and CI remediation ARE still allocated (unlike a
/// review, they are not pool-pinned) but the capability gate restricts them
/// to `claude`: `KindRequirements::for_kind` marks `CommandOutcomeObservation`
/// required-strict for these two execution kinds, and neither codex nor grok
/// declares it (mirrors the still-unmet "review and conflict resolution"
/// Phase 3 in the Codex/Grok driver design docs). Even a split that routes
/// everything else almost entirely away from claude must still land these on
/// claude, never codex or grok.
#[test]
fn conflict_resolution_and_ci_remediation_stay_on_claude_regardless_of_the_split() {
    let (_dir, db) = open_db();
    // grok=50, claude=30, codex=20 — claude is a minority share, so this
    // proves the restriction is the capability gate, not a split that
    // happens to favour claude.
    db.set_driver_traffic_split(DriverTrafficSplit::new(50, 30, 20))
        .unwrap();
    let product = create_test_product(&db);
    for kind in [ExecutionKind::ConflictResolution, ExecutionKind::CiRemediation] {
        let chore = create_test_chore(&db, &product.id, format!("{kind} chore"));
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(kind.clone())
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(
            decision.reason, REASON_ALLOCATION,
            "{kind} is not pool-pinned, so it must still go through allocation",
        );
        assert_eq!(
            decision.driver.as_deref(),
            Some(DRIVER_SLUG_CLAUDE),
            "{kind} must be capability-gated to claude even though claude is the minority share",
        );
    }
}

/// A live `tasks.driver = codex` pin must yield for ConflictResolution /
/// CiRemediation rather than record an explicit codex decision the spawn
/// path would then hard-fail on. Allocation places the row among the
/// eligible set (claude only) instead.
#[test]
fn conflict_resolution_and_ci_remediation_yield_a_codex_task_pin_to_claude() {
    let (_dir, db) = open_db();
    // Claude is a minority share so the result cannot be explained by the
    // split alone: only the capability gate (eligible set = {claude}) can
    // force the yield. codex=70 would own ordinary unpinned work.
    db.set_driver_traffic_split(DriverTrafficSplit::new(10, 20, 70))
        .unwrap();
    let product = create_test_product(&db);
    for kind in [ExecutionKind::ConflictResolution, ExecutionKind::CiRemediation] {
        let chore = create_test_chore(&db, &product.id, format!("{kind} pinned chore"));
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                driver: Some(DRIVER_SLUG_CODEX.to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(kind.clone())
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(
            decision.reason, REASON_ALLOCATION,
            "{kind}: codex pin must yield to allocation among eligible drivers, not record explicit",
        );
        assert_eq!(
            decision.driver.as_deref(),
            Some(DRIVER_SLUG_CLAUDE),
            "{kind}: yielded pin must allocate to claude",
        );
        // Spawn-time / events-socket resolver must also refuse to honour the
        // live pin for this execution kind.
        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
            Some(DRIVER_SLUG_CLAUDE),
            "{kind}: live codex pin must not win at lookup once the gate refuses it",
        );
    }
}

/// Same pin-yield as the task pin, but via `products.default_driver` — the
/// documented way to run a whole product on a non-default driver.
#[test]
fn conflict_resolution_and_ci_remediation_yield_a_codex_product_pin_to_claude() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(10, 20, 70))
        .unwrap();
    let product = create_test_product(&db);
    db.update_work_item(
        &product.id,
        WorkItemPatch {
            default_driver: Some(DRIVER_SLUG_CODEX.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    for kind in [ExecutionKind::ConflictResolution, ExecutionKind::CiRemediation] {
        let chore = create_test_chore(&db, &product.id, format!("{kind} product-pin chore"));
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(kind.clone())
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
        assert_eq!(
            decision.reason, REASON_ALLOCATION,
            "{kind}: product codex pin must yield to allocation, not record explicit",
        );
        assert_eq!(
            decision.driver.as_deref(),
            Some(DRIVER_SLUG_CLAUDE),
            "{kind}: yielded product pin must allocate to claude",
        );
        assert_eq!(
            db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
            Some(DRIVER_SLUG_CLAUDE),
            "{kind}: live product pin must not win at lookup once the gate refuses it",
        );
    }
}

/// An ordinary chore implementation with a codex pin still honours the pin
/// — the yield is execution-kind-specific, not a general pin disable.
#[test]
fn ordinary_chore_implementation_still_honours_a_codex_task_pin() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(100, 0, 0)).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "pinned ordinary chore");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            driver: Some(DRIVER_SLUG_CODEX.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let execution = create_ready_chore_execution(&db, &chore.id);
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.reason, REASON_EXPLICIT);
    assert_eq!(decision.driver.as_deref(), Some(DRIVER_SLUG_CODEX));
    assert_eq!(
        db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
        Some(DRIVER_SLUG_CODEX),
    );
}

/// Work that came from an automation runs on the automation pool, which
/// pins the same driver the review pool does
/// (`ClaudeCoordinator::execution_targets_automation_pool`). Its decision
/// records that fixed pool driver for the same reason it does for a review.
#[test]
fn automation_sourced_rows_record_the_pool_driver() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "automation-sourced chore");
    let automation = db
        .create_automation(boss_protocol::CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Fix clippy".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "America/Los_Angeles".to_owned(),
            },
            standing_instruction: "Fix any new clippy warnings.".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
        params![&automation.id, &chore.id],
    )
    .unwrap();
    drop(conn);

    let execution = create_ready_chore_execution(&db, &chore.id);
    let decision = db.get_execution_driver_decision(&execution.id).unwrap().unwrap();
    assert_eq!(decision.driver.as_deref(), Some(DRIVER_SLUG_CLAUDE));
    assert_eq!(decision.reason, REASON_POOL);
    assert_eq!(
        db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
        decision.driver.as_deref(),
        "the events-socket resolver must agree with the recorded pool decision \
         for an automation-sourced execution, even though its ExecutionKind \
         is not itself pool-bound",
    );
}

/// A live `tasks.driver = codex` pin on an automation-sourced row must not
/// win at lookup: `decide_execution_driver` and `get_execution_driver_slug`
/// must both short-circuit to the pool driver before either reads the pin.
#[test]
fn automation_sourced_rows_ignore_a_live_pin_at_lookup() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "automation-sourced pinned chore");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            driver: Some(DRIVER_SLUG_CODEX.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let automation = db
        .create_automation(boss_protocol::CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Fix clippy".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "America/Los_Angeles".to_owned(),
            },
            standing_instruction: "Fix any new clippy warnings.".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
        params![&automation.id, &chore.id],
    )
    .unwrap();
    drop(conn);

    let execution = create_ready_chore_execution(&db, &chore.id);
    assert_eq!(
        db.get_execution_driver_slug(&execution.id).unwrap().as_deref(),
        Some(DRIVER_SLUG_CLAUDE),
        "a live codex pin on an automation-sourced row must not win at lookup",
    );
}

/// A restricted eligible set holding no share at all is a loud failure, not
/// a quiet fallback to the engine default: zero means zero, and the default
/// driver may be exactly the one the kind refuses. Exercised through
/// `allocate_among` because no real work item can produce a restricted set
/// today — all three built-in drivers clear every kind's gate.
#[test]
fn allocation_fails_loudly_when_no_eligible_driver_holds_a_share() {
    let split = DriverTrafficSplit::new(0, 100, 0);
    let err = allocate_among(split, "task_zero", &TaskKind::Design, &[DRIVER_SLUG_GROK]).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("task_zero"), "{text}");
    assert!(text.contains("design"), "{text}");
    assert!(text.contains("0 share"), "{text}");
    assert!(
        !text.contains(DRIVER_SLUG_CLAUDE) || text.contains("claude=100"),
        "the error must not read as a fallback to claude: {text}",
    );
}

/// An empty eligible set is an error too, never the engine default.
#[test]
fn allocation_fails_loudly_when_nothing_is_eligible() {
    let err = allocate_among(DriverTrafficSplit::default(), "task_none", &TaskKind::Chore, &[]).unwrap_err();
    assert!(err.to_string().contains("no driver is eligible"), "{err}");
}

/// Restricting to a subset keeps allocation inside that subset for every
/// bucket, and keeps it deterministic — the same row, split and eligible
/// set always give the same answer.
#[test]
fn allocation_stays_inside_a_restricted_eligible_set() {
    let split = DriverTrafficSplit::new(20, 50, 30);
    let eligible = [DRIVER_SLUG_CODEX, DRIVER_SLUG_CLAUDE];
    for i in 0..200 {
        let id = format!("task_{i}");
        let driver = allocate_among(split, &id, &TaskKind::Investigation, &eligible).unwrap();
        assert!(eligible.contains(&driver), "{id} allocated to ineligible {driver}");
        assert_eq!(
            driver,
            allocate_among(split, &id, &TaskKind::Investigation, &eligible).unwrap(),
            "allocation must be deterministic for {id}",
        );
    }
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

#[test]
fn incompatible_model_override_falls_back_to_the_engine_default_driver() {
    let (_dir, db) = open_db();
    db.set_driver_traffic_split(DriverTrafficSplit::new(0, 0, 100)).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, &product.id, "model-pinned chore");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            model_override: Some("opus".to_owned()),
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
