use super::*;

struct ClaimLostPreservation<'a>(&'a WorkDb);
#[async_trait::async_trait]
impl Preservation for ClaimLostPreservation<'_> {
    async fn preserve(&self, execution: &WorkExecution, _: TerminalCause) -> Result<()> {
        self.0.clear_execution_workspace(&execution.id)?;
        anyhow::bail!("bookmark write failed")
    }
}

#[tokio::test]
async fn successful_retry_refreshes_legacy_teardown_items() {
    for independent in [false, true] {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "legacy-retry");
        let kind = boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;
        let legacy = "Teardown could not preserve this run's workspace edits (jj failed); lease was released anyway. The commit is not bookmarked; locate it in the shared jj store (for example with `jj op log`)";
        let body = if independent {
            format!("Independent inspection evidence\n- {legacy}")
        } else {
            format!("{legacy}. Existing preserved commit: `none`. Nothing was deleted. New workspace tip: unavailable.")
        };
        db.upsert_execution_attention(&execution.id, kind, "Inspect", &body)
            .unwrap();
        assert_eq!(
            apply_with(
                &db,
                &execution,
                TerminalCause::ProvenCompleted,
                Occupancy::Empty,
                Ok(()),
                Ok(())
            )
            .await,
            LeaseReleaseOutcome::Released
        );
        let items = db.list_attention_items(&execution.id).unwrap();
        let warning = items.iter().find(|item| item.kind == kind).unwrap();
        assert_eq!(warning.status, if independent { "open" } else { "resolved" });
        assert!(!warning.body_markdown.contains("not bookmarked"));
        assert!(!warning.body_markdown.contains("released anyway"));
        assert!(warning.body_markdown.contains("preservation succeeded"));
        assert_eq!(
            warning.body_markdown.contains("Independent inspection evidence"),
            independent
        );
    }
}

#[tokio::test]
async fn release_claim_error_still_reports_failed_preservation() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "claim-error");
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_release BEFORE UPDATE OF cube_lease_id ON work_executions
         WHEN NEW.cube_lease_id IS NULL BEGIN SELECT RAISE(ABORT, 'claim unavailable'); END;",
        )
        .unwrap();
    let outcome = apply_with(
        &db,
        &execution,
        TerminalCause::ProvenCompleted,
        Occupancy::Empty,
        Err("bookmark write failed"),
        Ok(()),
    )
    .await;
    assert!(matches!(outcome, LeaseReleaseOutcome::CubeFailed { error } if error.contains("claim unavailable")));
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.status == "open" && item.body_markdown.contains("bookmark write failed"))
    );
}

#[tokio::test]
async fn lost_release_claim_still_reports_failed_preservation() {
    let (_dir, db) = open_db();
    let execution = seed_terminal(&db, ExecutionStatus::Completed, "lost-claim");
    let outcome = apply_release(
        &db,
        &execution,
        "lost-claim",
        ReleaseEvidence {
            cause: TerminalCause::ProvenCompleted,
            pane: PaneStatus::Gone,
            occupancy: &FixedOccupancy(Occupancy::Empty),
            preservation: &ClaimLostPreservation(&db),
        },
        async { panic!("a lost claim must not call cube release") },
    )
    .await;
    assert_eq!(outcome, LeaseReleaseOutcome::NoLease);
    let items = db.list_attention_items(&execution.id).unwrap();
    let warning = items
        .iter()
        .find(|item| {
            item.kind == boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND
        })
        .unwrap();
    assert_eq!(warning.status, "open");
    assert!(warning.body_markdown.contains("bookmark write failed"));
    assert!(warning.body_markdown.contains("no longer held"));
}

#[tokio::test]
async fn successful_retry_refreshes_teardown_verdict_and_keeps_independent_evidence() {
    for independent in [false, true] {
        let (_dir, db) = open_db();
        let execution = seed_terminal(&db, ExecutionStatus::Completed, "retry");
        let kind = boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;
        if independent {
            db.upsert_execution_attention(&execution.id, kind, "Inspect", "Independent conflicting tip evidence")
                .unwrap();
        }
        let failed = apply_with(
            &db,
            &execution,
            TerminalCause::ProvenCompleted,
            Occupancy::Empty,
            Err("bookmark write failed"),
            Err(anyhow::anyhow!("release failed")),
        )
        .await;
        assert!(matches!(failed, LeaseReleaseOutcome::CubeFailed { .. }));
        let tip = boss_engine_recovery::execution_bookmark::PreservedTip {
            change_id: "change".into(),
            commit_id: "retained-commit".into(),
            count: 1,
        };
        db.record_preserved_tip(&execution.id, &tip).unwrap();
        let released = apply_with(
            &db,
            &execution,
            TerminalCause::ProvenCompleted,
            Occupancy::Empty,
            Ok(()),
            Ok(()),
        )
        .await;
        assert_eq!(released, LeaseReleaseOutcome::Released);
        let items = db.list_attention_items(&execution.id).unwrap();
        let warning = items.iter().find(|item| item.kind == kind).unwrap();
        assert_eq!(warning.status, if independent { "open" } else { "resolved" });
        assert!(warning.body_markdown.contains("retained-commit"));
        assert!(warning.body_markdown.contains("Released"));
        assert!(!warning.body_markdown.contains("still held"));
        assert!(!warning.body_markdown.contains("not bookmarked"));
        assert_eq!(
            warning.body_markdown.contains("Independent conflicting tip evidence"),
            independent
        );
    }
}
