use super::*;

#[tokio::test]
async fn a_preserved_work_refusal_blocks_without_retrying_or_starting_a_worker() {
    use boss_engine_recovery::execution_bookmark as recovery;
    use boss_protocol::{CreateChoreInput, CreateExecutionInput, ExecutionKind, TaskStatus, WorkItem};
    let fx = Fixture::with(workspace_slack(300)).await;
    let env = env(&fx, 1).await;
    let task = fx
        .db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(&env.product_id)
                .name("Recover a rewritten PR")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let prior = fx
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&task.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    fx.db
        .start_execution_run(
            &prior.id,
            "worker",
            "repo",
            "old-lease",
            "old-workspace",
            "/old-workspace",
        )
        .unwrap();
    fx.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'blocked' WHERE id = ?1",
            [&prior.id],
        )
        .unwrap();
    fx.db
        .record_worker_failure_with_basis(
            &prior.id,
            "needs a decision",
            boss_protocol::WorkerFailureBasis::DeclaredBlocked,
        )
        .unwrap();
    assert!(fx.db.claim_execution_workspace_release(&prior.id, "old-lease").unwrap());
    let head = "b".repeat(40);
    let tip = recovery::PreservedTip {
        change_id: "k".repeat(32),
        commit_id: "c".repeat(40),
        count: 1,
    };
    fx.db
        .record_execution_bookmark(
            &recovery::ExecutionBookmark::builder()
                .execution_id(&prior.id)
                .host_id(HOST)
                .repo_path("/spoke/repo".into())
                .base_commit("a".repeat(40))
                .build(),
        )
        .unwrap();
    fx.db.record_preserved_tip(&prior.id, &tip).unwrap();
    let next = fx
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&task.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    fx.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET pr_url = 'https://github.com/acme/widget/pull/99' WHERE id = ?1",
            [&next.id],
        )
        .unwrap();
    fx.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'todo', autostart = 1 WHERE id = ?1",
            [&task.id],
        )
        .unwrap();
    let detail = format!(
        "preserved work is not contained in PR head {head}: preserved/{} at {}; current bookmarks: destination={head}",
        prior.id, tip.commit_id
    );
    let expected = detail.clone();
    let peer = fx.spoke(Box::new(move |c| {
        if matches!(&c.body, Some(command::Body::Workspace(op)) if op.kind == i32::from(WorkspaceKind::RestorePreserved)) {
            let command::Body::Workspace(op) = c.body.as_ref().unwrap() else { unreachable!() };
            let request: recovery::HostRecoveryRequest = serde_json::from_slice(&op.request_json).unwrap();
            assert!(request.pr_bound);
            let wire = recovery::uncontained_refusal_to_wire(&recovery::uncontained_preserved_work(&detail)).unwrap();
            let mut result = refused(c, ResultCode::Rejected, &detail);
            result.value = Some(workspace(c, serde_json::from_slice(&wire).unwrap()));
            Some(result)
        } else { answer(c) }
    }), None, 0).await;
    // Keep the coordinator's normal, non-empty pre-start retry delays.
    env.coordinator.kick();
    let settled = tokio::time::timeout(Duration::from_secs(20), async {
        while fx.db.get_execution(&next.id).unwrap().status != ExecutionStatus::Failed {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "refusal did not settle: execution={:?}, task={:?}, commands={:?}",
        fx.db.get_execution(&next.id).unwrap(),
        fx.db.get_work_item(&task.id).unwrap(),
        peer.commands()
    );
    let WorkItem::Chore(blocked) = fx.db.get_work_item(&task.id).unwrap() else {
        panic!("expected chore")
    };
    assert_eq!(blocked.status, TaskStatus::Blocked);
    assert_eq!(blocked.blocked_reason.as_deref(), Some("execution_recovery_failed"));
    assert!(blocked.blocked_detail.as_deref().unwrap().contains(&expected));
    assert!(!blocked.autostart);
    let failed = fx.db.get_execution(&next.id).unwrap();
    assert!(failed.dispatch_not_before.is_none(), "no queued retry: {failed:?}");
    assert_eq!(fx.db.list_executions(None).unwrap().len(), 2);
    // Another scheduler pass cannot retry the refused recovery.
    env.coordinator.kick();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let commands = peer.commands();
    assert_eq!(commands.iter().filter(|c| matches!(&c.body, Some(command::Body::Workspace(op)) if op.kind == i32::from(WorkspaceKind::RestorePreserved))).count(), 1);
    assert!(!commands.iter().any(|c| matches!(c.body, Some(command::Body::Spawn(_)))));
    env.coordinator.abort_driving_runs();
}

#[tokio::test]
async fn restarted_unfinished_launches_keep_reconciling_every_reply_shape() {
    for shape in ["unknown", "indeterminate", "late_ok", "unconfirmed_stop"] {
        let fx = Fixture::with(short(300)).await;
        let env = env(&fx, 1).await;
        let execution = triage(&fx, &env, shape);
        if shape == "unconfirmed_stop" {
            fx.signals.0.store(false, Ordering::SeqCst);
        }
        let peer = fx
            .spoke(
                Box::new(move |c| match &c.body {
                    Some(command::Body::Spawn(_)) => match shape {
                        "unknown" => Some(refused(c, ResultCode::Unknown, "unknown spawn")),
                        "indeterminate" => Some(ok(c, Some(pane(PaneLife::Indeterminate)))),
                        "late_ok" => None,
                        _ => answer(c),
                    },
                    Some(command::Body::Adopt(_)) => Some(refused(c, ResultCode::OutcomeUncertain, "cannot observe")),
                    Some(command::Body::Stop(_)) => None,
                    _ => answer(c),
                }),
                None,
                0,
            )
            .await;
        env.coordinator.kick();
        until("launch attention", || attention_open(&fx, &execution.id)).await;
        let next = restarted(&fx, &env.coordinator, 1).await;
        if shape == "late_ok" {
            until("spawn command", || {
                peer.commands()
                    .iter()
                    .any(|c| matches!(c.body, Some(command::Body::Spawn(_))))
            })
            .await;
            let spawn = peer
                .commands()
                .into_iter()
                .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
                .unwrap();
            peer.answer_late(&spawn, answer(&spawn).unwrap()).await;
            until("reply committed before sweep", || {
                fx.db.host_command_result(HOST, &spawn.command_id).unwrap().is_some()
            })
            .await;
        }
        assert!(fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap());
        next.reconcile_ended_in_doubt_spokes().await;
        if shape == "late_ok" {
            until("launch finalized", || {
                fx.db.active_run_ids_for_execution(&execution.id).unwrap().is_empty()
                    && !attention_open(&fx, &execution.id)
            })
            .await;
            assert!(!fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap());
        } else {
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(attention_open(&fx, &execution.id), "{shape}");
            assert!(fx.db.spoke_assignment_needs_recovery(&execution.id).unwrap(), "{shape}");
        }
        assert_never_failed(&fx, &execution.id);
        next.abort_driving_runs();
    }
}

#[tokio::test]
async fn terminal_release_in_flight_is_retried_without_a_stop() {
    let fx = Fixture::with(short(300)).await;
    let env = env(&fx, 1).await;
    let execution = triage(&fx, &env, "release in flight");
    let peer = fx
        .spoke(
            Box::new(|c| {
                if matches!(&c.body, Some(command::Body::Workspace(op))
            if op.kind == i32::from(WorkspaceKind::Release))
                {
                    None
                } else if matches!(c.body, Some(command::Body::Adopt(_))) {
                    Some(ok(c, Some(pane(PaneLife::Alive))))
                } else {
                    answer(c)
                }
            }),
            None,
            0,
        )
        .await;
    env.coordinator.kick();
    until("launch finalized", || {
        fx.db.open_spoke_assignment_for_pane_pid(HOST, 4242).unwrap().is_some()
            && fx.db.active_run_ids_for_execution(&execution.id).unwrap().is_empty()
    })
    .await;
    fx.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = ?1 WHERE id = ?2",
            ["completed", &execution.id],
        )
        .unwrap();
    let adapter = fx.adapter(HOST, &execution.id).await.unwrap();
    let release = tokio::spawn(async move { adapter.release_workspace("lease-1").await });
    until("release command stored", || released(&peer.commands())).await;
    env.coordinator.reconcile_ended_in_doubt_spokes().await;
    assert!(!stopped(&peer.commands()), "sweep must not stop during normal release");
    assert!(release.await.unwrap().is_err(), "unanswered release stays pending");
    assert!(
        !fx.db
            .spoke_assignment_for_execution(&execution.id)
            .unwrap()
            .unwrap()
            .resolved
    );
}
