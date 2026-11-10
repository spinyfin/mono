use super::*;

#[test]
fn open_remediation_snapshot_is_compact_complete_and_recovers_exhaustion() {
    let (_dir, path) = disk_db_path("open-remediation");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "P", Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "open PR");
    let pr_url = "https://github.com/foo/bar/pull/42";
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.increment_ci_attempts_used(&chore.id).unwrap();
    db.mark_chore_blocked_ci_failure_exhausted(&chore.id, pr_url).unwrap();
    let snapshot = db.open_remediation_state().unwrap();
    assert_eq!(snapshot.len(), 1, "exhaustion must recover even without an attempt");
    assert_eq!(snapshot[0].status, "exhausted");
    assert_eq!(snapshot[0].extra["attempts_used"], "1");
    assert_eq!(snapshot[0].extra["budget"], "3");

    let mut latest = String::new();
    for n in 0..205 {
        let attempt = db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: product.id.clone(),
                work_item_id: chore.id.clone(),
                pr_url: pr_url.into(),
                pr_number: 42,
                head_branch: "feature".into(),
                head_sha_at_trigger: format!("head-{n}"),
                attempt_kind: "fix".into(),
                consumes_budget: 1,
                failed_checks: "large diagnostic blob".repeat(1000),
                failure_kind: "pr_branch_ci".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .unwrap();
        // Force deterministic timestamp ordering within the same second.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE ci_remediations SET created_at = ?1 WHERE id = ?2",
                params![format!("{n:06}"), attempt.id],
            )
            .unwrap();
        latest = attempt.id;
    }
    let snapshot = db.open_remediation_state().unwrap();
    assert_eq!(
        snapshot.len(),
        2,
        "latest row plus authoritative card state, not history"
    );
    assert_eq!(snapshot[0].id, latest);
    assert_eq!(snapshot[0].extra["attempts_used"], "1");
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(!json.contains("large diagnostic blob"));
    assert!(!json.contains("failed_checks"));
    assert!(!json.contains("log_excerpt"));

    db.mark_ci_remediation_succeeded(&latest, None).unwrap();
    db.retry_ci_remediation_for_work_item(&chore.id).unwrap();
    let snapshot = db.open_remediation_state().unwrap();
    assert_eq!(snapshot[0].status, "succeeded");
    assert!(snapshot[0].finished_at.is_some());
    assert_eq!(snapshot[1].status, "clear");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(db.open_remediation_state().unwrap().is_empty());
}

/// The pushed `card_state` must equal the open-PR snapshot's `ci_state` row
/// for every parent shape, including an `in_review` parent whose failure
/// signal is still armed after a fix revision spawned.
#[test]
fn ci_card_state_matches_open_snapshot_for_every_parent_shape() {
    let (_dir, path) = disk_db_path("ci-card-state");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "P", Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "open PR");
    let pr_url = "https://github.com/foo/bar/pull/42";
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..Default::default()
        },
    )
    .unwrap();

    let check = |expected: &str| {
        let projected = db
            .remediation_entry(
                EngineAttemptListEntry::builder()
                    .id("attempt")
                    .product_id(product.id.clone())
                    .created_at("2026-01-01")
                    .kind("ci")
                    .pr_url(pr_url)
                    .status("failed")
                    .work_item_id(chore.id.clone())
                    .extra(Default::default())
                    .build(),
            )
            .extra["card_state"]
            .clone();
        let snapshot = db.open_remediation_state().unwrap();
        let state_row = snapshot.iter().find(|r| r.kind == "ci_state").unwrap();
        assert_eq!(projected, expected);
        assert_eq!(state_row.status, expected);
    };

    check("clear");

    db.mark_chore_blocked_ci_failure(&chore.id, pr_url, None).unwrap();
    check("in_flight");

    // Fix revision spawned: parent back to in_review, signal stays armed.
    db.clear_chore_blocked_ci_failure(&chore.id, pr_url).unwrap();
    db.record_ci_failure_in_flight(&chore.id, "att-1").unwrap();
    check("in_flight");

    db.mark_chore_blocked_ci_failure_exhausted(&chore.id, pr_url).unwrap();
    check("exhausted");

    // A higher-priority scalar reason occupies the slot; the exhausted
    // signal is still armed and the chip must stay sticky.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'review_feedback' WHERE id = ?1",
            params![chore.id],
        )
        .unwrap();
    check("exhausted");

    db.clear_ci_failure_signal_only(&chore.id).unwrap();
    check("clear");
}
