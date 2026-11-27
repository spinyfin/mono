use super::helpers::*;

#[tokio::test]
async fn rebounce_decline_gates_end_the_owed_merge_request() {
    for (gate, detail) in [
        ("maintenance", "automatic PR maintenance is off"),
        ("budget", "CI-fix budget is spent"),
        ("already_blocked", "CI-fix budget is spent"),
        ("suppressed", "suppressed"),
        ("episode", "already had a CI fix"),
        ("episode_poll", "already had a CI fix"),
        ("episode_insert", "already had a CI fix"),
    ] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/500";
        let (product, chore) = make_in_review(&db, gate, pr);
        let publisher = RecordingPublisher::default();
        let requested = db.record_merge_intent(&chore, pr, Some("head")).unwrap();
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE merge_intents SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![(now - 10).to_string(), requested.id],
            )
            .unwrap();
        db.note_merge_intent_left_queue(&chore, pr, "failed_checks", now)
            .unwrap();
        match gate {
            "maintenance" => {
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE products SET auto_pr_maintenance_enabled = 0 WHERE id = ?1",
                        [&product],
                    )
                    .unwrap();
            }
            "budget" | "already_blocked" => {
                db.set_ci_attempt_budget(&chore, Some(0)).unwrap();
                if gate == "already_blocked" {
                    db.mark_chore_blocked_ci_failure_exhausted(&chore, pr).unwrap();
                }
            }
            "suppressed" => {
                db.connect().unwrap().execute(
                    "INSERT INTO ci_failure_suppressions (work_item_id, head_sha, created_at) VALUES (?1, 'head', ?2)",
                    rusqlite::params![chore, now.to_string()],
                ).unwrap();
            }
            "episode" | "episode_poll" | "episode_insert" => {
                let attempt = db
                    .insert_ci_remediation(
                        crate::work::CiRemediationInsertInput::builder()
                            .product_id(&product)
                            .work_item_id(&chore)
                            .pr_url(pr)
                            .pr_number(500)
                            .head_branch("feature")
                            .head_sha_at_trigger("mq:head")
                            .attempt_kind("fix")
                            .consumes_budget(1)
                            .failed_checks("[]")
                            .failure_kind("merge_queue_rebounce")
                            .before_commit_sha("synthetic")
                            .build(),
                    )
                    .unwrap()
                    .unwrap();
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE ci_remediations SET created_at = ?1 WHERE id = ?2",
                        rusqlite::params![(now - 20).to_string(), attempt.id],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        if gate == "episode_insert" {
            on_queue_side_failure_detected(
                &db,
                crate::queue_ci_contract::test_context(&publisher),
                &candidate(&product, &chore, pr),
                None,
                QueueSideFailureEpisode {
                    discriminator: "mq:head",
                    suppression_discriminator: "head",
                    before_commit_sha: Some("synthetic"),
                    queue_base_sha: None,
                    failure_kind: "merge_queue_rebounce",
                    labels: &[],
                },
                &[],
            )
            .await;
        } else if gate == "episode_poll" {
            db.connect()
                .unwrap()
                .execute("UPDATE tasks SET pr_head_sha = 'head' WHERE id = ?1", [&chore])
                .unwrap();
            let events = std::collections::HashMap::from([(
                pr.to_owned(),
                vec![crate::MergeQueueDequeueEvent {
                    reason: "failed_checks".into(),
                    pr_head_oid: Some("head".into()),
                    before_commit_oid: Some("synthetic".into()),
                    queue_base_oid: None,
                    created_at: Some(chrono::DateTime::from_timestamp(now, 0).unwrap().to_rfc3339()),
                }],
            )]);
            crate::merge_queue::check_merge_queue_rebounce(
                &db,
                crate::queue_ci_contract::test_context(&publisher),
                &candidate(&product, &chore, pr),
                &events,
                &mut crate::SweepOutcome::default(),
            )
            .await;
        } else {
            on_merge_queue_rebounce_detected(
                &db,
                crate::queue_ci_contract::test_context(&publisher),
                &candidate(&product, &chore, pr),
                "head",
                "synthetic",
                None,
                &one_failure(),
            )
            .await;
        }
        assert!(db.active_merge_intent(&chore, pr).unwrap().is_none(), "{gate}");
        let shown = task(&db, &chore).merge_intent.unwrap();
        assert_eq!(shown.status, "needs_review", "{gate}");
        assert!(shown.detail.unwrap().contains(detail), "{gate}");
    }
}
