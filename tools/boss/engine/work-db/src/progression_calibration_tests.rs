use super::*;
use crate::test_support::{create_test_chore_manual, create_test_product};
use boss_engine_storage_values::progression::{METADATA_KEY_RUN_TIMEOUT_SECS, METADATA_KEY_STUCK_AFTER_SECS};
use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus, StuckBucket, StuckReport, StuckRow};

#[path = "progression_calibration_identity_tests.rs"]
mod identity;

const NOW: i64 = 20_000 * DAY;
fn db() -> WorkDb {
    WorkDb::open(PathBuf::from(":memory:")).unwrap()
}
fn snapshots(db: &WorkDb, count: i64) {
    let report = StuckReport::builder()
        .generated_at_epoch_s(NOW)
        .minimum_age_secs(0)
        .questions_filed(0)
        .unconverted_declared_blockers(0)
        .rows(
            (0..4)
                .map(|i| {
                    StuckRow::builder()
                        .work_item_id(format!("item{i}"))
                        .name("stuck")
                        .age_in_state_secs(100)
                        .bucket(StuckBucket::EngineStuck)
                        .classification("merge_conflict")
                        .last_ledger_basis("engine")
                        .age_is_lower_bound(false)
                        .build()
                })
                .collect(),
        )
        .build();
    for day in 1..=count {
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO task_stuck_daily_snapshots(day,report_json) VALUES (?1,?2)",
                params![NOW / DAY - day, serde_json::to_string(&report).unwrap()],
            )
            .unwrap();
    }
}
fn seed(db: &WorkDb) {
    snapshots(db, 7);
    let product = create_test_product(db).id;
    let task = create_test_chore_manual(db, &product, "measured").id;
    let revision = create_test_chore_manual(db, &product, "revision").id;
    let start = NOW - 10 * DAY;
    let mut run_ids = vec![];
    for (index, duration) in [3600, 7200, 10800].into_iter().enumerate() {
        let run = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&task)
                    .kind(ExecutionKind::Progression)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at=?2, started_at=?2, finished_at=?3 WHERE id=?1",
            params![
                run.id,
                start + index as i64 * DAY,
                start + index as i64 * DAY + duration
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES (?1,?2)",
            params![
                format!("progression.fingerprint.{}", run.id),
                format!("fingerprint-{index}")
            ],
        )
        .unwrap();
        run_ids.push(run.id);
    }
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET kind='revision', parent_task_id=?2, created_via='progression:revise',
        created_at=?3 WHERE id=?1",
        params![revision, task, start],
    )
    .unwrap();
    conn.execute("DELETE FROM task_status_audit", []).unwrap();
    for (item, stamp, old, new) in [
        (task.as_str(), start, "backlog", "active"),
        (task.as_str(), start + 10000, "active", "in_review"),
        (revision.as_str(), start + 20000, "active", "in_review"),
        ("parked_interval", start + 30000, "backlog", "active"),
        ("parked_interval", start + 50000, "active", "in_review"),
    ] {
        conn.execute(
            "INSERT INTO task_status_audit(work_item_id,property,old_value,new_value,actor,changed_at,basis)
            VALUES (?1,'status',?2,?3,'engine',?4,'measured')",
            params![item, old, new, stamp],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO task_status_audit(work_item_id,property,new_value,actor,changed_at,basis)
        VALUES ('parked_interval','parked_at','1','engine',?1,'parking')",
        [start + 40000],
    )
    .unwrap();
    // Long human waits and migration lower bounds must not enter the grace.
    conn.execute("INSERT INTO task_status_audit(work_item_id,property,old_value,new_value,actor,changed_at,basis,age_is_lower_bound)
        VALUES (?1,'status','active','in_review','human',?2,'human',1)",params![task,NOW-1]).unwrap();
    for index in 0..5 {
        let proposal = if index < 2 {
            format!("ladder:q{index}")
        } else {
            format!("proposal{index}")
        };
        conn.execute("INSERT INTO operator_questions(id,work_item_id,execution_id,proposal_id,question_json,status,created_at,answered_at)
            VALUES (?1,?2,?3,?4,'{}','answered',?5,?6)",
            params![format!("q{index}"),task,run_ids[0],proposal,start,start+4*DAY]).unwrap();
    }
    drop(conn);
    // Low-churn items: the high-churn item above must not set the budget.
    for other in 0..3 {
        let item = create_test_chore_manual(db, &product, format!("quiet{other}")).id;
        for index in 0..2 {
            let id = attempt(
                db,
                &product,
                &item,
                &format!("q{other}h{index}"),
                "fix",
                "merge_queue_rebounce",
            );
            db.mark_ci_remediation_running(&id, "lease", "workspace", "worker")
                .unwrap();
            db.mark_ci_remediation_failed(&id, "fix_failed").unwrap();
            dates(db, &id, start + index * 11000, start + index * 11000 + 1000);
        }
    }
    for index in 0..4 {
        let id = attempt(db, &product, &task, &format!("head{index}"), "fix", "pr_branch_ci");
        db.mark_ci_remediation_abandoned_with_head(&id, "new_head_sha_inflight", Some(&format!("head{}", index + 1)))
            .unwrap();
        dates(db, &id, start + index * 11000, start + index * 11000 + 1000);
    }
}

fn value(report: &ProgressionCalibration, key: &str) -> Option<u64> {
    report.limits[key].value
}
#[test]
fn insufficient_history_and_missing_samples_remain_fallbacks() {
    let db = db();
    snapshots(&db, 6);
    let report = db.calibrate_progression(NOW).unwrap();
    assert_eq!(report.snapshot_days, 6);
    for (i, key) in CALIBRATED_KEYS.iter().enumerate() {
        assert_eq!(
            value(&report, key),
            ProgressionSettings::default().calibration_values()[i]
        );
        assert_eq!(report.limits[*key].source, "fallback");
    }
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO task_stuck_daily_snapshots SELECT day-7, report_json FROM task_stuck_daily_snapshots LIMIT 1",
        [],
    )
    .unwrap();
    drop(conn);
    let next = db.calibrate_progression(NOW + DAY).unwrap();
    assert_eq!(next.snapshot_days, 7);
    assert_eq!(next.limits["run_timeout_secs"].source, "fallback");
    assert_eq!(next.limits["run_timeout_secs"].basis, "no eligible measurement samples");
}

#[test]
fn seeded_ledger_computes_formulas_publishes_report_and_caches_the_day() {
    let db = db();
    seed(&db);
    let report = db.calibrate_progression(NOW).unwrap();
    for (key, expected) in [
        ("pool_size", 3),
        ("sweep_interval_secs", 100000),
        ("stuck_after_secs", 30000),
        ("cooldown_secs", 60000),
        ("max_creates_per_item_window", 6),
        ("run_timeout_secs", 32400),
        ("expected_questions_per_week", 2),
    ] {
        assert_eq!(value(&report, key), Some(expected), "{key}");
        assert_eq!(report.limits[key].source, "computed", "{key}");
        assert_eq!(
            db.progression_limit(&format!("progression.{key}"), 0).unwrap(),
            expected
        );
    }
    // Seeded questions were answered after four days; the placeholder is a
    // ceiling, so the reminder stays at three days here.
    assert_eq!(value(&report, "remind_after_secs"), Some(3 * DAY as u64));
    assert_eq!(report.limits["remind_after_secs"].source, "computed");
    assert!(report.limits["sweep_interval_secs"].basis.contains("WARNING"));
    assert_eq!(report.inputs["automatic_pause_p99_secs"], 10000);
    assert_eq!(report.inputs["engine_ladder_questions"], 2);
    assert_eq!(report.inputs["progression_questions"], 3);
    assert_eq!(
        report
            .fingerprint_changes_per_item
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert!(report.fingerprint_risk.contains("1/1"));
    assert!(
        db.stuck_report(0, NOW, &|_| false)
            .unwrap()
            .progression_calibration
            .is_some()
    );
    assert_eq!(db.calibrate_progression(NOW + 100).unwrap().computed_at, NOW);
    // Once the seeded measurements roll out of the window, defaults return.
    let expired = db.calibrate_progression(NOW + 29 * DAY).unwrap();
    assert_eq!(expired.snapshot_days, 0);
    assert_eq!(expired.limits["run_timeout_secs"].source, "fallback");
    assert_eq!(db.progression_limit(METADATA_KEY_RUN_TIMEOUT_SECS, 0).unwrap(), 1800);
}

#[test]
fn explicit_override_even_equal_to_placeholder_wins_and_restart_can_remove_it() {
    let db = db();
    seed(&db);
    let settings = ProgressionSettings::builder()
        .explicit_overrides(0xff)
        .expected_questions_per_week(1)
        .build();
    db.configure_progression_calibration(&settings).unwrap();
    let report = db.calibrate_progression(NOW).unwrap();
    for (index, key) in CALIBRATED_KEYS.iter().enumerate() {
        assert_eq!(report.limits[*key].source, "overridden");
        assert_eq!(value(&report, key), settings.calibration_values()[index]);
    }
    db.configure_progression_calibration(&ProgressionSettings::default())
        .unwrap();
    assert_eq!(
        db.calibrate_progression(NOW).unwrap().limits["pool_size"].source,
        "computed"
    );
}

// Lifecycle transitions use production APIs; SQL only moves their clock.
fn attempt(db: &WorkDb, product: &str, item: &str, head: &str, kind: &str, failure: &str) -> String {
    db.insert_ci_remediation_with_queue_base(
        CiRemediationInsertInput {
            product_id: product.into(),
            work_item_id: item.into(),
            pr_url: "https://github.com/example/repo/pull/1".into(),
            pr_number: 1,
            head_branch: "branch".into(),
            head_sha_at_trigger: head.into(),
            attempt_kind: kind.into(),
            consumes_budget: 1,
            failed_checks: r#"[{"name":"test","conclusion":"failure"}]"#.into(),
            failure_kind: failure.into(),
            before_commit_sha: None,
        },
        (failure != "pr_branch_ci").then_some("queue-base"),
    )
    .unwrap()
    .unwrap()
    .id
}

fn dates(db: &WorkDb, id: &str, created: i64, finished: i64) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE ci_remediations SET created_at=?2,
                finished_at=CASE WHEN status IN ('failed','abandoned','succeeded') THEN ?3 ELSE NULL END WHERE id=?1",
            params![id, created, finished],
        )
        .unwrap();
}

fn pause_inputs(db: &WorkDb) -> ProgressionCalibration {
    super::progression_calibration_data::measure(&db.connect().unwrap(), NOW, 7 * DAY).unwrap()
}

#[test]
fn red_episode_continuity_uses_production_lifecycle_facts() {
    for (reason, next_head, queue, expected) in [
        ("new_head_sha_inflight", "new", false, 1),
        ("legacy_new_head", "new", false, 0),
        ("new_head_sha_inflight", "mq:new", true, 0),
        ("superseded_by_resolved_ci", "new", false, 0),
        ("verified_requeue", "old", false, 0),
        ("owner_abandon", "old", false, 0),
        ("merge_queue_trigger_superseded", "old", false, 0),
        ("superseded_by_conflict", "old", false, 0),
        ("revision_create_failed", "old", false, 0),
        ("failed", "external", false, 0),
        ("failed", "old", false, 1),
        ("succeeded", "new", false, 0),
    ] {
        let db = db();
        let product = create_test_product(&db).id;
        let item = create_test_chore_manual(&db, &product, "CI episode").id;
        let start = NOW - 20 * DAY;
        let first = attempt(&db, &product, &item, "old", "retrigger", "pr_branch_ci");
        match reason {
            "failed" => {
                db.mark_ci_remediation_failed(&first, "fix_failed").unwrap();
            }
            "succeeded" => {
                db.mark_ci_remediation_succeeded(&first, None).unwrap();
            }
            "superseded_by_resolved_ci" => {
                db.abandon_active_ci_remediations_for_work_item(&item, reason, false)
                    .unwrap();
            }
            "legacy_new_head" => {
                db.mark_ci_remediation_abandoned(&first, "new_head_sha_inflight")
                    .unwrap();
            }
            "new_head_sha_inflight" => {
                db.mark_ci_remediation_abandoned_with_head(&first, reason, Some("new"))
                    .unwrap();
            }
            _ => {
                db.mark_ci_remediation_abandoned(&first, reason).unwrap();
            }
        }
        dates(&db, &first, start, start + 10);
        let next = attempt(
            &db,
            &product,
            &item,
            next_head,
            "fix",
            if queue { "merge_queue_rebounce" } else { "pr_branch_ci" },
        );
        dates(
            &db,
            &next,
            start + if queue { 10 * DAY } else { 110 },
            start + if queue { 10 * DAY + 120 } else { 120 },
        );
        assert_eq!(
            pause_inputs(&db).inputs["automatic_pause_samples"],
            expected,
            "{reason}/{next_head}"
        );
    }
}

#[test]
fn resolved_ci_and_overlapping_work_invalidate_even_continuous_heads() {
    for exclusion in [
        "signal",
        "status",
        "reason",
        "parent_run",
        "revision_run",
        "queued_run",
        "human_wait",
        "parking",
        "autostart",
        "trunk_queue",
    ] {
        let db = db();
        let product = create_test_product(&db).id;
        let item = create_test_chore_manual(&db, &product, "continuous head").id;
        let start = NOW - 20 * DAY;
        let first = attempt(&db, &product, &item, "old", "fix", "pr_branch_ci");
        db.mark_ci_remediation_abandoned_with_head(&first, "new_head_sha_inflight", Some("new"))
            .unwrap();
        dates(&db, &first, start, start + 100);
        let next = attempt(
            &db,
            &product,
            &item,
            "new",
            "fix",
            if exclusion == "trunk_queue" {
                "trunk_queue_eviction"
            } else {
                "pr_branch_ci"
            },
        );
        dates(&db, &next, start + 10 * DAY, start + 10 * DAY + 100);
        match exclusion {
            "signal" => {
                db.connect()
                    .unwrap()
                    .execute(
                        "INSERT INTO task_blocked_signals(work_item_id,reason,created_at,cleared_at)
                    VALUES (?1,'ci_failure',?2,?3)",
                        params![item, start, start + DAY],
                    )
                    .unwrap();
            }
            "status" | "reason" | "parking" | "autostart" => {
                let (property, old, new) = match exclusion {
                    "status" => ("status", "blocked", "in_review"),
                    "reason" => ("blocked_reason", "ci_failure", ""),
                    "parking" => ("parked_at", "", "1"),
                    _ => ("autostart", "1", "0"),
                };
                db.connect().unwrap().execute("INSERT INTO task_status_audit(work_item_id,property,old_value,new_value,actor,changed_at,basis)
                    VALUES (?1,?2,?3,?4,'engine',?5,'observed')",
                    params![item,property,old,new,start+DAY]).unwrap();
            }
            "trunk_queue" => {}
            _ => {
                let revision = create_test_chore_manual(&db, &product, "revision").id;
                db.set_ci_remediation_revision_task_id(&first, &revision).unwrap();
                let run = db
                    .create_execution(
                        CreateExecutionInput::builder()
                            .work_item_id(if exclusion == "revision_run" { &revision } else { &item })
                            .kind(if exclusion == "human_wait" {
                                ExecutionKind::Progression
                            } else {
                                ExecutionKind::ChoreImplementation
                            })
                            .status(ExecutionStatus::Completed)
                            .build(),
                    )
                    .unwrap();
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE work_executions SET created_at=?2,started_at=?3,finished_at=?4 WHERE id=?1",
                        params![
                            run.id,
                            start - 100,
                            if exclusion == "queued_run" {
                                start + DAY
                            } else {
                                start - 100
                            },
                            start + 2 * DAY
                        ],
                    )
                    .unwrap();
                if exclusion == "human_wait" {
                    db.connect()
                        .unwrap()
                        .execute(
                            "INSERT INTO execution_wait_history(execution_id,changed_at) VALUES (?1,?2)",
                            params![run.id, start + DAY],
                        )
                        .unwrap();
                }
            }
        }
        assert_eq!(pause_inputs(&db).inputs["automatic_pause_samples"], 0, "{exclusion}");
    }
}

#[test]
fn abandoned_then_green_then_queue_rebounce_is_not_a_pause() {
    let db = db();
    let product = create_test_product(&db).id;
    let item = create_test_chore_manual(&db, &product, "green before queue").id;
    let start = NOW - 20 * DAY;
    let first = attempt(&db, &product, &item, "old", "fix", "pr_branch_ci");
    db.mark_ci_remediation_abandoned_with_head(&first, "new_head_sha_inflight", Some("new"))
        .unwrap();
    db.abandon_active_ci_remediations_for_work_item(&item, "superseded_by_resolved_ci", false)
        .unwrap();
    dates(&db, &first, start, start + 100);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status='in_review',pr_url='https://github.com/example/repo/pull/1' WHERE id=?1",
            [&item],
        )
        .unwrap();
    db.mark_chore_blocked_ci_signal(
        &item,
        "https://github.com/example/repo/pull/1",
        Some(&first),
        "ci_failure",
    )
    .unwrap();
    assert!(db.clear_ci_failure_signal_only(&item).unwrap());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE task_blocked_signals SET created_at=?2,cleared_at=?3 WHERE work_item_id=?1",
            params![item, start, start + DAY],
        )
        .unwrap();
    let next = attempt(&db, &product, &item, "mq:new", "fix", "merge_queue_rebounce");
    dates(&db, &next, start + 10 * DAY, start + 10 * DAY + 100);
    assert_eq!(pause_inputs(&db).inputs["automatic_pause_samples"], 0);
}

#[test]
fn continuous_red_pauses_drop_one_extreme_outlier() {
    let db = db();
    let product = create_test_product(&db).id;
    let item = create_test_chore_manual(&db, &product, "red gaps").id;
    let mut at = NOW - 20 * DAY;
    for index in 0..13 {
        let id = attempt(&db, &product, &item, &format!("head{index}"), "fix", "pr_branch_ci");
        db.mark_ci_remediation_abandoned_with_head(&id, "new_head_sha_inflight", Some(&format!("head{}", index + 1)))
            .unwrap();
        dates(&db, &id, at, at + 10);
        at += if index == 11 { 10 + 10 * DAY } else { 110 };
    }
    let report = pause_inputs(&db);
    assert_eq!(report.inputs["automatic_pause_samples"], 12);
    assert_eq!(report.inputs["automatic_pause_p99_secs"], 100);
}

#[test]
fn configuration_and_snapshots_survive_measurement_and_metadata_errors() {
    let db = db();
    db.set_metadata(REPORT_KEY, "corrupt json").unwrap();
    assert!(
        db.stuck_report(0, NOW, &|_| false)
            .unwrap()
            .progression_calibration
            .is_none()
    );
    db.configure_progression_calibration(&ProgressionSettings::default())
        .unwrap();
    db.set_metadata(CONFIG_KEY, "invalid config").unwrap();
    assert!(db.calibrate_progression(NOW).is_err());
    assert_eq!(db.progression_limit(METADATA_KEY_STUCK_AFTER_SECS, 0).unwrap(), 7200);
    let snapshot = db.stuck_daily_snapshot(NOW, &|_| false).unwrap();
    assert!(snapshot.progression_calibration.is_none());
}

#[test]
fn daily_snapshot_omits_calibration_while_live_report_includes_it() {
    let db = db();
    seed(&db);
    db.calibrate_progression(NOW).unwrap();
    assert!(
        db.stuck_report(0, NOW, &|_| false)
            .unwrap()
            .progression_calibration
            .is_some()
    );
    assert!(
        db.stuck_daily_snapshot(NOW, &|_| false)
            .unwrap()
            .progression_calibration
            .is_none()
    );
    let json: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT report_json FROM task_stuck_daily_snapshots WHERE day=?1",
            [NOW / DAY],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!json.contains("fingerprint_changes_per_item"));
}

#[test]
fn remind_after_follows_fast_answers_and_never_exceeds_the_placeholder() {
    let db = db();
    seed(&db);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE operator_questions SET answered_at=CAST(created_at AS INTEGER)+7200",
            [],
        )
        .unwrap();
    let report = db.calibrate_progression(NOW).unwrap();
    assert_eq!(report.inputs["answer_latency_p75_secs"], 7200);
    assert_eq!(value(&report, "remind_after_secs"), Some(7200));
    assert_eq!(report.limits["remind_after_secs"].source, "computed");
    assert_eq!(report.inputs["remediation_attempts_p99"], 2);
}
