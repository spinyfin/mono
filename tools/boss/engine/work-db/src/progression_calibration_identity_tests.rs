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
    // Several churning items defeat outlier trimming alone.
    for item_index in 0..3 {
        let item = create_test_chore_manual(&db, &product, format!("churn{item_index}")).id;
        for index in 0..20 {
            let genuine = index < 4;
            let status = if genuine {
                ["pending", "failed", "succeeded", "abandoned"][index as usize]
            } else if index % 4 == 0 {
                "pending"
            } else {
                "abandoned"
            };
            let reason = if genuine || index % 4 < 2 {
                "owner_abandon"
            } else {
                [
                    "new_head_sha_inflight",
                    "merge_queue_trigger_superseded",
                    "superseded_by_resolved_ci",
                    "superseded_by_conflict",
                ][(index / 4) as usize % 4]
            };
            let started = genuine || index % 4 >= 2;
            let revision = revision_execution(&db, &product, started);
            let id = attempt(&db, &product, &item, &format!("head{index}"), "fix", "pr_branch_ci");
            db.set_ci_remediation_revision_task_id(&id, &revision).unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE ci_remediations SET status=?2,failure_reason=?3,
                    consumes_budget=?4,attempt_kind=?5 WHERE id=?1",
                params![
                    id,
                    status,
                    reason,
                    if !genuine && index % 4 == 3 { 0 } else { 1 },
                    if !genuine && index % 4 == 3 { "retrigger" } else { "fix" }
                ],
            )
            .unwrap();
            // A started non-budget retrigger must be excluded independently
            // of supersession, including when its ledger also has a start.
            if !genuine && index % 4 == 3 {
                conn.execute(
                    "UPDATE ci_remediations SET status='succeeded',failure_reason=NULL,
                        started_at=?2 WHERE id=?1",
                    params![id, NOW - 5 * DAY],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO conflict_resolutions(id,product_id,work_item_id,pr_url,pr_number,head_branch,base_branch,
                    status,created_at,revision_task_id,failure_reason)
                 VALUES (?1,?2,?3,'https://github.com/example/repo/pull/1',1,'branch','main',?4,?5,?6,?7)",
                params![
                    format!("conflict-{item_index}-{index}"),
                    product,
                    item,
                    status,
                    NOW - 5 * DAY + index,
                    revision,
                    if genuine || !started {
                        "owner_abandon"
                    } else {
                        "superseded_stale_head"
                    }
                ],
            )
            .unwrap();
            drop(conn);
            dates(&db, &id, NOW - 5 * DAY + index, NOW - 5 * DAY + index + 1);
        }
    }
    let report = db.calibrate_progression(NOW).unwrap();
    // Each item has four CI and four conflict attempts. Pending ledger rows
    // count after execution starts; owner-abandoned started work also counts.
    assert_eq!(report.inputs["remediation_attempts_p99"], 8);
    assert_eq!(value(&report, "max_creates_per_item_window"), Some(24));
}

#[test]
fn legacy_and_revision_start_evidence_count_each_ledger_row_once() {
    let db = super::db();
    snapshots(&db, 7);
    let product = create_test_product(&db).id;
    for item_index in 0..3 {
        let item = create_test_chore_manual(&db, &product, format!("legacy{item_index}")).id;
        for index in 0..2 {
            let revision = (index == 1).then(|| revision_execution(&db, &product, true));
            let id = attempt(&db, &product, &item, &format!("head{index}"), "fix", "pr_branch_ci");
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE ci_remediations SET started_at=?2,created_at=?2,revision_task_id=?3 WHERE id=?1",
                params![id, NOW - 5 * DAY, revision],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO conflict_resolutions(id,product_id,work_item_id,pr_url,pr_number,head_branch,base_branch,
                    status,created_at,started_at,revision_task_id)
                 VALUES (?1,?2,?3,'https://github.com/example/repo/pull/1',1,'branch','main',
                    'succeeded',?4,?4,?5)",
                params![
                    format!("legacy-{item_index}-{index}"),
                    product,
                    item,
                    NOW - 5 * DAY,
                    revision
                ],
            )
            .unwrap();
        }
    }
    let report = db.calibrate_progression(NOW).unwrap();
    assert_eq!(report.inputs["remediation_attempts_p99"], 4);
    assert_eq!(value(&report, "max_creates_per_item_window"), Some(12));
}
