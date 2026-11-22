use super::*;

fn finish(fx: &Fx, revision: &str) {
    fx.sql(&format!(
        "UPDATE tasks SET status = 'in_review' WHERE id = '{revision}';
         UPDATE work_executions SET status = 'completed' WHERE work_item_id = '{revision}'"
    ));
}

#[test]
fn finished_progression_revision_allows_another_fix_on_a_red_or_conflicted_pr() {
    for reason in ["ci_failure", "merge_conflict", "awaiting_operator_answer"] {
        let fx = fx();
        let parent = fx.pr_item("PR needs another fix", "fail");
        let first_run = fx.run(&parent, FINGERPRINT, "S6_ci_failure");
        let first = fx.applied(&first_run, ProposalKind::Revise, json!({"brief": "First fix"}));
        finish(&fx, &first);
        fx.sql(&format!(
            "UPDATE tasks SET status = 'blocked', blocked_reason = '{reason}' WHERE id = '{parent}'"
        ));
        let run = fx.run(&parent, "new-head", "S13_revision_chain");
        let next = fx.applied(&run, ProposalKind::Revise, json!({"brief": "Fix the new failure"}));
        let execution = fx.executions(&next).remove(0);
        assert_eq!(execution.status, ExecutionStatus::Ready);
        assert_eq!(
            fx.db.claim_execution_for_dispatch(&execution.id).unwrap(),
            DispatchClaimOutcome::Won
        );
        assert_eq!(fx.task(&parent).blocked_reason.as_deref(), Some(reason));
        assert_eq!(fx.task(&first).status, TaskStatus::InReview);
    }
}

#[test]
fn finished_revision_can_be_archived_without_green_ci() {
    for status in ["in_review", "active"] {
        let fx = fx();
        let parent = fx.pr_item("Conflicted PR", "fail");
        let revision = fx.revision_of(&parent, "Delivered fix", status);
        fx.db
            .create_execution(
                CreateExecutionInput::builder()
                    .created_via("test")
                    .work_item_id(&revision)
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        let run = fx.run(&parent, FINGERPRINT, "S13_revision_chain");
        fx.applied(
            &run,
            ProposalKind::ArchiveRevision,
            json!({"revision": revision, "reason": "Delivered; superseded by the next fix"}),
        );
        assert_eq!(fx.task(&revision).status, TaskStatus::Archived);
        assert_eq!(fx.task(&parent).ci_required_state.as_deref(), Some("fail"));
    }
}

#[test]
fn restarted_in_review_revision_cannot_be_retired_or_duplicated_by_progression() {
    for status in [
        ExecutionStatus::Queued,
        ExecutionStatus::Ready,
        ExecutionStatus::WaitingDependency,
        ExecutionStatus::Claimed,
        ExecutionStatus::Running,
        ExecutionStatus::WaitingHuman,
    ] {
        let fx = fx();
        let parent = fx.pr_item("Red PR", "fail");
        let first = fx.run(&parent, FINGERPRINT, "S6_ci_failure");
        let revision = fx.applied(&first, ProposalKind::Revise, json!({"brief": "First fix"}));
        finish(&fx, &revision);
        fx.db
            .create_execution(
                CreateExecutionInput::builder()
                    .created_via("test")
                    .work_item_id(&revision)
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(status)
                    .build(),
            )
            .unwrap();
        let run = fx.run(&parent, "new-head", "S13_revision_chain");
        assert!(
            fx.refused(&run, ProposalKind::Revise, json!({"brief": "Another fix"}))
                .contains("open progression revision")
        );
        assert!(
            fx.refused(
                &run,
                ProposalKind::ArchiveRevision,
                json!({"revision": revision, "reason": "superseded"})
            )
            .contains("live or pending work")
        );
        assert_eq!(fx.task(&revision).status, TaskStatus::InReview);
    }
}

#[test]
fn archive_preserves_finished_revisions_with_live_read_only_executions() {
    for kind in [ExecutionKind::PrReview, ExecutionKind::Progression] {
        let fx = fx();
        let parent = fx.pr_item("Red PR", "fail");
        let revision = fx.revision_of(&parent, "Delivered revision", "in_review");
        fx.db
            .create_execution(
                CreateExecutionInput::builder()
                    .created_via("test")
                    .work_item_id(&revision)
                    .kind(kind)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();
        let run = fx.run(&parent, FINGERPRINT, "S13_revision_chain");
        assert!(
            fx.refused(
                &run,
                ProposalKind::ArchiveRevision,
                json!({"revision": revision, "reason": "superseded"})
            )
            .contains("live or pending work")
        );
        assert_eq!(fx.task(&revision).status, TaskStatus::InReview);
    }
}
