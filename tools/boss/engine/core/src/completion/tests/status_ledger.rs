use super::*;

#[tokio::test]
async fn completion_producers_carry_their_basis_to_the_failure_transaction() {
    for producer in [
        "declared_blocked",
        "nudge_breaker",
        "delivered_without_pr",
        "foreign_pr_binding",
    ] {
        let workspace = tempdir().unwrap();
        let (_dir, db, product, chore, execution) = fixture(workspace.path());
        let foreign_pr = "https://github.com/spinyfin/mono/pull/123";
        let detector = if producer == "foreign_pr_binding" {
            let owner = create_test_chore(&db, product, "PR owner");
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
                    rusqlite::params![owner.id, foreign_pr],
                )
                .unwrap();
            StubPrDetector::ok(Some(foreign_pr))
        } else {
            StubPrDetector::ok(None)
        };
        let handler = TestHarness::new(db.clone(), detector)
            .handler
            .with_max_unproductive_nudges(1);
        let outcome = match producer {
            "declared_blocked" => {
                handler
                    .finalize_declared_run_done(&execution, boss_protocol::RunDoneOutcome::Blocked)
                    .await
            }
            "delivered_without_pr" => {
                handler
                    .finalize_declared_run_done(&execution, boss_protocol::RunDoneOutcome::Delivered)
                    .await
            }
            "nudge_breaker" => {
                assert!(matches!(handler.on_stop(&execution).await, StopOutcome::AwaitingInput));
                handler.on_stop(&execution).await
            }
            _ => handler.on_stop(&execution).await,
        };
        assert!(!matches!(outcome, StopOutcome::DbError), "{producer}: {outcome:?}");
        let report = db
            .stuck_report(0, boss_engine_utils::epoch_time::now_epoch_secs())
            .unwrap();
        let row = report.rows.iter().find(|row| row.work_item_id == chore).unwrap();
        assert_eq!(row.failure_basis.as_deref(), Some(producer));
        assert_eq!(row.bucket, boss_protocol::StuckBucket::EngineStuck);
        let waived: Option<String> = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT progression_grace_waived_at FROM tasks WHERE id = ?1",
                [&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            waived.is_some(),
            matches!(producer, "declared_blocked" | "nudge_breaker")
        );
    }
}
