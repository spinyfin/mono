use super::*;

#[test]
fn named_settings_publish_distinct_values_and_preserve_each_override_bit() {
    let settings = ProgressionSettings::builder()
        .pool_size(2)
        .sweep_interval_secs(301)
        .stuck_after_secs(7201)
        .cooldown_secs(21601)
        .max_creates_per_item_window(4)
        .run_timeout_secs(1801)
        .remind_after_secs(3601)
        .expected_questions_per_week(17)
        .build();
    let expected = [2, 301, 7201, 21601, 4, 1801, 3601, 17];
    let db = super::db();
    db.configure_progression_calibration(&settings).unwrap();
    let report = db.calibrate_progression(NOW).unwrap();
    for ((setting, key), expected) in CALIBRATED_SETTINGS.iter().zip(CALIBRATED_KEYS).zip(expected) {
        assert_eq!(setting.key, key);
        assert_eq!(setting.metadata_key, format!("progression.{key}"));
        assert_eq!(value(&report, key), Some(expected));
        assert_eq!(db.progression_limit(setting.metadata_key, 0).unwrap(), expected);
    }
    // A placeholder override must affect only its named field, even when
    // measurements would otherwise produce a different value.
    for setting in CALIBRATED_SETTINGS {
        let db = super::db();
        seed(&db);
        db.configure_progression_calibration(
            &ProgressionSettings::builder()
                .explicit_overrides(setting.override_bit)
                .build(),
        )
        .unwrap();
        let report = db.calibrate_progression(NOW).unwrap();
        for candidate in CALIBRATED_SETTINGS {
            assert_eq!(
                report.limits[candidate.key].source == "overridden",
                candidate.key == setting.key
            );
        }
    }
}

#[test]
fn superseded_unstarted_and_nonbudget_rows_cannot_inflate_create_budget() {
    let db = super::db();
    seed(&db);
    let product = create_test_product(&db).id;
    // Multiple churning items defeat trimming alone. Each has three genuine
    // attempts, followed by many rows that must not enter the measurement.
    for item_index in 0..3 {
        let item = create_test_chore_manual(&db, &product, format!("churn{item_index}")).id;
        for index in 0..18 {
            let id = attempt(&db, &product, &item, &format!("head{index}"), "fix", "pr_branch_ci");
            if index < 3 || index % 3 != 0 {
                db.mark_ci_remediation_running(&id, "lease", "workspace", "worker")
                    .unwrap();
            }
            if index < 3 {
                db.mark_ci_remediation_failed(&id, "fix_failed").unwrap();
            } else if index % 3 == 1 {
                db.mark_ci_remediation_abandoned_with_head(&id, "new_head_sha_inflight", Some("next"))
                    .unwrap();
            } else if index % 3 == 2 {
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE ci_remediations SET consumes_budget=0, attempt_kind='retrigger' WHERE id=?1",
                        [&id],
                    )
                    .unwrap();
            }
            dates(&db, &id, NOW - 5 * DAY + index, NOW - 5 * DAY + index + 1);
            // Conflict rows include both churn-guard abandonment before start
            // and supersession after start, as well as never-started pending work.
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO conflict_resolutions(id,product_id,work_item_id,pr_url,pr_number,head_branch,base_branch,
                    status,created_at,started_at)
                 VALUES (?1,?2,?3,'https://github.com/example/repo/pull/1',1,'branch','main',?4,?5,?6)",
                params![
                    format!("conflict-{item_index}-{index}"),
                    product,
                    item,
                    if index % 3 == 0 { "pending" } else { "abandoned" },
                    NOW - 5 * DAY + index,
                    (index % 3 == 2).then_some(NOW - 5 * DAY + index)
                ],
            )
            .unwrap();
        }
        // A started, successful conflict resolution is a genuine fourth attempt.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE conflict_resolutions SET status='succeeded',started_at=created_at WHERE id=?1",
                [format!("conflict-{item_index}-0")],
            )
            .unwrap();
    }
    let report = db.calibrate_progression(NOW).unwrap();
    assert_eq!(report.inputs["remediation_attempts_p99"], 4);
    assert_eq!(value(&report, "max_creates_per_item_window"), Some(12));
}
