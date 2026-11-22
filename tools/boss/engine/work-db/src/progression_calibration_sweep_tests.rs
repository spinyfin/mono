use super::*;

#[test]
fn nights_and_weekends_do_not_determine_nomination_latency() {
    let db = db();
    seed(&db);
    let conn = db.connect().unwrap();
    // Daily bursts separated by overnight gaps and a three-day weekend.
    // Use a separate item so these writes do not alter remediation episodes.
    for day in [19, 18, 17, 16, 15, 12, 11] {
        for offset in [0, 10, 20, 30] {
            conn.execute(
                "INSERT INTO task_status_audit(work_item_id,property,old_value,new_value,actor,changed_at,basis)
                VALUES ('burst','status','active','in_review','engine',?1,'observed')",
                [NOW - day * DAY + offset],
            )
            .unwrap();
        }
    }
    // Dense PR-poll and deferral writes in the weekend gap must not shrink the
    // p99: they are the same properties stuck-report ageing ignores.
    for hour in 1..70 {
        let stamp = NOW - 15 * DAY + 30 + hour * 3600;
        for property in ["pr_mergeable_state", "ci_required_state", "deferred"] {
            conn.execute(
                "INSERT INTO task_status_audit(work_item_id,property,old_value,new_value,actor,changed_at,basis)
                VALUES ('burst',?1,'old','new','engine',?2,'observed')",
                params![property, stamp],
            )
            .unwrap();
        }
    }
    drop(conn);
    let report = db.calibrate_progression(NOW).unwrap();
    assert_eq!(report.inputs["ledger_interval_p99_secs"], 3 * DAY as u64 - 30);
    assert_eq!(report.inputs["automatic_pause_p99_secs"], 10000);
    let sweep = &report.limits["sweep_interval_secs"];
    assert_eq!(sweep.value, Some(3000));
    assert_eq!(sweep.source, "computed");
    assert!(!sweep.basis.contains("WARNING"));
    assert_eq!(db.progression_limit(METADATA_KEY_SWEEP_INTERVAL_SECS, 0).unwrap(), 3000);
}

#[test]
fn sweep_uses_effective_grace_with_floor_override_and_diagnostic_bound() {
    for (grace, expected) in [(100, 300), (7200, 720), (12349, 1234), (72000, 7200)] {
        let db = db();
        seed(&db);
        let settings = ProgressionSettings::builder()
            .stuck_after_secs(grace)
            .explicit_overrides(STUCK_AFTER_SECS_SETTING.override_bit)
            .build();
        db.configure_progression_calibration(&settings).unwrap();
        let report = db.calibrate_progression(NOW).unwrap();
        assert_eq!(report.limits["stuck_after_secs"].source, "overridden");
        let sweep = &report.limits["sweep_interval_secs"];
        assert_eq!(sweep.value, Some(expected));
        assert_eq!(sweep.source, "computed");
        // An unusually large grace still warns; the bound is not a cap.
        assert_eq!(sweep.basis.contains("WARNING"), expected > 3600);
        let settings = ProgressionSettings::builder()
            .stuck_after_secs(grace)
            .sweep_interval_secs(301)
            .build();
        db.configure_progression_calibration(&settings).unwrap();
        let overridden = db.calibrate_progression(NOW).unwrap();
        assert_eq!(value(&overridden, "sweep_interval_secs"), Some(301));
        assert_eq!(overridden.limits["sweep_interval_secs"].source, "overridden");
    }
}

#[test]
fn sweep_keeps_fallback_without_measured_or_configured_grace() {
    let db = db();
    snapshots(&db, 7);
    let report = db.calibrate_progression(NOW).unwrap();
    assert_eq!(value(&report, "sweep_interval_secs"), Some(300));
    assert_eq!(report.limits["sweep_interval_secs"].source, "fallback");
}

#[test]
fn sweep_uses_placeholder_floor_of_measured_grace() {
    let db = db();
    seed(&db);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE ci_remediations SET finished_at=CAST(created_at AS INTEGER)+10900
            WHERE failure_kind='pr_branch_ci'",
            [],
        )
        .unwrap();
    let report = db.calibrate_progression(NOW).unwrap();
    assert_eq!(report.inputs["automatic_pause_p99_secs"], 100);
    assert_eq!(value(&report, "stuck_after_secs"), Some(7200));
    assert_eq!(value(&report, "sweep_interval_secs"), Some(720));
}
