use super::*;

#[test]
fn blocked_root_redispatch_continues_the_conflicting_pr_once() {
    let fx = fx();
    let item = fx.pr_item("Conflicting PR", "success");
    let pr_url = fx.task(&item).pr_url;
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict',
         pr_mergeable_state = 'CONFLICTING' WHERE id = '{item}'"
    ));
    let run = fx.run(&item, FINGERPRINT, "S5_merge_conflict");
    let execution_id = fx.applied(
        &run,
        ProposalKind::Redispatch,
        json!({"brief_addendum": "Resolve the conflict on the existing PR."}),
    );
    let execution = fx.db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.kind, ExecutionKind::RevisionImplementation);
    assert_eq!(execution.status, ExecutionStatus::Ready);
    assert_eq!(execution.pr_url, pr_url);
    let revision = fx.task(&execution.work_item_id);
    assert_eq!(revision.kind, TaskKind::Revision);
    assert_eq!(revision.parent_task_id.as_deref(), Some(item.as_str()));
    assert!(revision.pr_url.is_none());
    assert!(
        revision
            .description
            .contains("Resolve the conflict on the existing PR.")
    );
    assert_eq!(fx.task(&item).pr_url, pr_url);
    assert_eq!(fx.actor(&revision.id), LAST_STATUS_ACTOR_PROGRESSION);
    assert_eq!(fx.created_via(&execution_id), Some(format!("progression:{run}")));
    assert!(fx.executions(&item).is_empty());
    assert_eq!(fx.executions(&revision.id).len(), 1);

    let second = fx.run(&item, "fp-2", "S5_merge_conflict");
    let refusal = fx.refused(&second, ProposalKind::Redispatch, json!({}));
    assert!(refusal.contains("redispatch"), "{refusal}");
    let children: i64 = fx
        .db
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1", [&item], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(children, 1);
    assert_eq!(fx.executions(&revision.id).len(), 1);

    fx.sql(&format!(
        "UPDATE work_executions SET status = 'completed' WHERE id = '{execution_id}';
         UPDATE tasks SET status = 'done' WHERE id = '{}'",
        revision.id
    ));
    let third = fx.run(&item, "fp-3", "S5_merge_conflict");
    let next = fx.applied(&third, ProposalKind::Redispatch, json!({}));
    assert_ne!(
        next, execution_id,
        "completed revision work resets the redispatch bound"
    );
    assert_eq!(fx.db.get_execution(&next).unwrap().pr_url, pr_url);
}

#[test]
fn blocked_pr_redispatch_refuses_closed_and_merged_prs_atomically() {
    for state in [PrOpenState::ClosedUnmerged, PrOpenState::Merged] {
        let mut fx = fx();
        let item = fx.pr_item("Closed PR", "success");
        fx.fail(&item, "declared_blocked");
        let before = fx.task(&item);
        let run = fx.run(&item, FINGERPRINT, STUCK);
        fx.checker = FakePrStateChecker::always(state);
        fx.refused(&run, ProposalKind::Redispatch, json!({"brief_addendum": "Retry"}));
        assert_eq!(fx.task(&item).description, before.description);
        assert_eq!(fx.task(&item).pr_url, before.pr_url);
        assert!(fx.executions(&item).is_empty());
        assert!(fx.actions(&item).is_empty());
        let children: i64 = fx
            .db
            .connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1", [&item], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(children, 0);
    }
}

#[test]
fn blocked_pr_redispatch_sequences_behind_an_existing_writer() {
    let fx = fx();
    let item = fx.pr_item("Busy PR", "success");
    fx.fail(&item, "declared_blocked");
    let existing = fx.revision_of(&item, "Existing writer", "active");
    let run = fx.run(&item, FINGERPRINT, STUCK);
    let next = fx.applied(&run, ProposalKind::Redispatch, json!({}));
    let execution = fx.db.get_execution(&next).unwrap();
    assert_eq!(execution.status, ExecutionStatus::WaitingDependency);
    assert_eq!(execution.pr_url, fx.task(&item).pr_url);
    assert!(
        crate::work_dependencies::query_edge(&fx.db.connect().unwrap(), &execution.work_item_id, &existing, "blocks")
            .unwrap()
            .is_some()
    );
}
