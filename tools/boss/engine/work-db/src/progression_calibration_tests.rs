use super::*;
use crate::test_support::{create_test_chore_manual, create_test_product};
use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus, StuckBucket, StuckReport, StuckRow};

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
    for index in 0..4 {
        conn.execute(
            "INSERT INTO ci_remediations(id,product_id,work_item_id,pr_url,pr_number,head_branch,head_sha_at_trigger,
            attempt_kind,consumes_budget,failed_checks,status,created_at,finished_at)
            VALUES (?1,?2,?3,'https://example.com/pr',1,'branch',?1,'fix',1,'[]','completed',?4,?5)",
            params![
                format!("remediation{index}"),
                product,
                task,
                start + index * 11000,
                start + index * 11000 + 1000
            ],
        )
        .unwrap();
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
        ("max_creates_per_item_window", 12),
        ("run_timeout_secs", 32400),
        ("expected_questions_per_week", 2),
    ] {
        assert_eq!(value(&report, key), Some(expected), "{key}");
        assert_eq!(report.limits[key].source, "computed", "{key}");
        assert_eq!(db.progression_limit(key, 0).unwrap(), expected);
    }
    assert_eq!(value(&report, "remind_after_secs"), Some(3 * DAY as u64));
    assert_eq!(report.limits["remind_after_secs"].source, "fallback");
    assert!(report.limits["sweep_interval_secs"].basis.contains("WARNING"));
    assert_eq!(report.inputs["automatic_pause_max_secs"], 10000);
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
    assert_eq!(db.progression_limit("run_timeout_secs", 0).unwrap(), 1800);
}

#[test]
fn explicit_override_even_equal_to_placeholder_wins_and_restart_can_remove_it() {
    let db = db();
    seed(&db);
    let settings = ProgressionSettings::builder()
        .explicit_overrides(0xff)
        .expected_questions_per_week(1)
        .build();
    db.configure_progression_calibration(&settings, NOW).unwrap();
    let report = db.calibrate_progression(NOW).unwrap();
    for (index, key) in CALIBRATED_KEYS.iter().enumerate() {
        assert_eq!(report.limits[*key].source, "overridden");
        assert_eq!(value(&report, key), settings.calibration_values()[index]);
    }
    db.configure_progression_calibration(&ProgressionSettings::default(), NOW)
        .unwrap();
    assert_eq!(
        db.calibrate_progression(NOW).unwrap().limits["pool_size"].source,
        "computed"
    );
}

#[test]
fn coding_merge_and_execution_human_waits_do_not_inflate_automatic_pauses() {
    let db = db();
    seed(&db);
    let baseline = super::super::progression_calibration_data::measure(&db.connect().unwrap(), NOW, 7 * DAY).unwrap();
    let product = create_test_product(&db).id;
    let item = create_test_chore_manual(&db, &product, "human waits").id;
    let run = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&item)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    let start = NOW - 20 * DAY;
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET created_at=?2, started_at=?2, finished_at=?3 WHERE id=?1",
        params![run.id, start, NOW - 1],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO execution_wait_history(execution_id,changed_at) VALUES (?1,?2)",
        params![run.id, start + DAY],
    )
    .unwrap();
    for (stamp, old, new) in [
        (start, "backlog", "active"),
        (start + DAY, "active", "in_review"),
        (NOW - 1, "in_review", "done"),
    ] {
        conn.execute(
            "INSERT INTO task_status_audit(work_item_id,property,old_value,new_value,actor,changed_at,basis)
            VALUES (?1,'status',?2,?3,'engine',?4,'observed')",
            params![item, old, new, stamp],
        )
        .unwrap();
    }
    for index in 0..2 {
        conn.execute(
            "INSERT INTO ci_remediations(id,product_id,work_item_id,pr_url,pr_number,head_branch,
            head_sha_at_trigger,attempt_kind,consumes_budget,failed_checks,status,created_at,finished_at)
            VALUES (?1,?2,?3,'https://example.com/pr',1,'branch',?1,'fix',1,'[]','completed',?4,?5)",
            params![
                format!("human-remediation{index}"),
                product,
                item,
                start + index * 10 * DAY,
                start + index * 10 * DAY + 1000
            ],
        )
        .unwrap();
    }
    drop(conn);
    let report = db.calibrate_progression(NOW).unwrap();
    assert_eq!(
        report.inputs["automatic_pause_samples"],
        baseline.inputs["automatic_pause_samples"]
    );
    assert_eq!(
        report.inputs["automatic_pause_max_secs"],
        baseline.inputs["automatic_pause_max_secs"]
    );
    assert_eq!(value(&report, "stuck_after_secs"), Some(30000));
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
    db.configure_progression_calibration(&ProgressionSettings::default(), NOW)
        .unwrap();
    db.set_metadata(CONFIG_KEY, "invalid config").unwrap();
    assert!(db.calibrate_progression(NOW).is_err());
    assert_eq!(db.progression_limit("stuck_after_secs", 0).unwrap(), 7200);
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
