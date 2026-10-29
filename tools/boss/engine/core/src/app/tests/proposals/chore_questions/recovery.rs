use super::*;
use boss_protocol::{CreateChoreInput, CreateRevisionInput, ExecutionStatus, TaskKind, TaskStatus};
use rusqlite::params;

#[tokio::test]
async fn conflict_answer_recovery_preserves_writer_exclusion() {
    for writer_status in ["failed", "claimed", "running", "waiting_human"] {
        let (state, _dir, execution, writer) = invariant::fixture(&TaskKind::Revision, &TaskStatus::Blocked, false);
        let db = &state.work_db;
        let checker = crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open);
        let source = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(&writer)
                    .description("Conflict fix awaiting an answer")
                    .created_via(format!("{}test", boss_protocol::CREATED_VIA_MERGE_CONFLICT_PREFIX))
                    .autostart(false)
                    .build(),
                &checker,
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET work_item_id = ?2 WHERE id = ?1",
                params![execution, source.id],
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = ?1",
                [&source.id],
            )
            .unwrap();
        let prior = db
            .request_execution(
                boss_protocol::RequestExecutionInput::builder()
                    .work_item_id(&writer)
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                params![prior.id, writer_status],
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = ?1",
                [&writer],
            )
            .unwrap();
        submitted(
            call_with_peer(
                &state,
                Some(std::process::id() as libc::pid_t),
                submit_request(&execution, ProposalKind::RunDone, operator_question_payload()),
            )
            .await,
        );
        let attention: String = db.connect().unwrap().query_row(
            "SELECT a.id FROM attentions a JOIN attention_groups g ON g.id = a.group_id WHERE g.source_task_id = ?1",
            [&source.id], |r| r.get(0),
        ).unwrap();
        let group = db
            .answer_attention(&attention, Some("yes".into()), false, false)
            .unwrap();
        let result = db
            .action_attention_group(&group.id, false, &Default::default(), &checker)
            .unwrap();
        let recovery = &result.produced_work_item_ids[0];
        let consumer = db.list_executions(Some(recovery)).unwrap().remove(0);
        if writer_status != "failed" {
            assert_eq!(db.gating_prereqs_for(recovery).unwrap(), vec![writer.clone()]);
            assert_eq!(consumer.status, ExecutionStatus::WaitingDependency);
            assert_eq!(
                db.claim_execution_for_dispatch(&consumer.id).unwrap(),
                crate::work::DispatchClaimOutcome::Rejected
            );
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
                    [&prior.id],
                )
                .unwrap();
            assert!(db.try_unblock_dependency_if_resolved(recovery).unwrap());
        }
        assert!(db.gating_prereqs_for(recovery).unwrap().is_empty());
        assert_eq!(db.get_execution(&consumer.id).unwrap().status, ExecutionStatus::Ready);
        assert_eq!(
            db.claim_execution_for_dispatch(&consumer.id).unwrap(),
            crate::work::DispatchClaimOutcome::Won
        );
        let status: String = db
            .connect()
            .unwrap()
            .query_row("SELECT status FROM tasks WHERE id = ?1", [&writer], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "blocked");
    }
}

#[tokio::test]
async fn blocked_revision_answer_preserves_prerequisites_and_later_revision_order() {
    for conflict in [false, true] {
        check_prerequisites_and_later_revision_order(conflict).await;
    }
}

async fn check_prerequisites_and_later_revision_order(conflict: bool) {
    let (state, _dir, execution, source) = invariant::fixture(&TaskKind::Revision, &TaskStatus::Blocked, false);
    let db = &state.work_db;
    if conflict {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET created_via = ?2 WHERE id = ?1",
                params![
                    source,
                    format!("{}test", boss_protocol::CREATED_VIA_MERGE_CONFLICT_PREFIX)
                ],
            )
            .unwrap();
    }
    let product: String = db
        .connect()
        .unwrap()
        .query_row("SELECT product_id FROM tasks WHERE id = ?1", [&source], |r| r.get(0))
        .unwrap();
    let prerequisite = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product)
                .name("Independent prerequisite")
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.connect().unwrap().execute("INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at) VALUES (?1, ?2, 'blocks', '1')",
        params![source, prerequisite.id]).unwrap();
    let checker = crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open);
    let later = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&source)
                .description("Later revision")
                .build(),
            &checker,
        )
        .unwrap();
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    let attention: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT a.id FROM attentions a JOIN attention_groups g ON g.id = a.group_id WHERE g.source_task_id = ?1",
            [&source],
            |r| r.get(0),
        )
        .unwrap();
    let group = db
        .answer_attention(&attention, Some("yes".into()), false, false)
        .unwrap();
    let result = db
        .action_attention_group(&group.id, false, &Default::default(), &checker)
        .unwrap();
    let recovery = &result.produced_work_item_ids[0];
    assert_eq!(db.gating_prereqs_for(recovery).unwrap(), vec![prerequisite.id.clone()]);
    assert!(db.gating_prereqs_for(&later.id).unwrap().contains(&source));
    let consumer = db.list_executions(Some(recovery)).unwrap().remove(0);
    assert_eq!(consumer.status, ExecutionStatus::WaitingDependency);
    assert_eq!(
        db.claim_execution_for_dispatch(&consumer.id).unwrap(),
        crate::work::DispatchClaimOutcome::Rejected
    );
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&prerequisite.id])
        .unwrap();
    assert!(db.try_unblock_dependency_if_resolved(recovery).unwrap());
    assert_eq!(db.get_execution(&consumer.id).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(
        db.claim_execution_for_dispatch(&consumer.id).unwrap(),
        crate::work::DispatchClaimOutcome::Won
    );
    let status: (String, String, String) = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT status, blocked_reason, blocked_detail FROM tasks WHERE id = ?1",
            [&source],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        status,
        ("blocked".into(), "ci_failed".into(), "Existing blocker".into())
    );
    assert!(!db.try_unblock_dependency_if_resolved(&later.id).unwrap());
}
