use super::*;
use boss_protocol_spoke::v1::{Cursor, WorkspaceOp};
use prost::Message;

#[tokio::test]
async fn same_workspace_basename_does_not_cross_stores() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    fx.serve_legacy();
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx.spoke(Box::new(standard), None, 0).await;
    lease(&adapter).await;
    let unbound = fx.provider_adapter(HOST).await;
    let error = unbound
        .create_change(Path::new("/ssh/workspaces/worker"), "legacy")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("legacy transport was used"));
    assert_eq!(fx.legacy_calls(), ["create_change"]);
    assert_eq!(peer.commands().len(), 1);

    let record = ExecutionBookmark {
        execution_id: "other_execution".into(),
        repo_path: PATH.into(),
        host_id: HOST.into(),
        base_commit: None,
        pr_head_before: None,
    };
    for error in [
        adapter
            .confirm_workspace_recovery(&record, Path::new(PATH), None, None)
            .await
            .unwrap_err(),
        adapter
            .restore_preserved_work(&record, Path::new(PATH), None)
            .await
            .unwrap_err(),
    ] {
        assert!(error.to_string().contains("recovery record belongs to execution"));
    }
    assert_eq!(peer.commands().len(), 1);
}

#[tokio::test]
async fn lost_release_reply_resolves_on_reconnect_without_releasing_again() {
    for kind in [Kind::Release, Kind::ReleaseWithReset, Kind::ForceRelease] {
        let fx = Fixture::with(short(300)).await;
        fx.ready(HOST);
        let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
        let first = fx
            .spoke(
                Box::new(|c| {
                    if workspace_kind(c) == Some(Kind::Lease) {
                        standard(c)
                    } else {
                        None
                    }
                }),
                None,
                0,
            )
            .await;
        lease(&adapter).await;
        assert!(release(&adapter, kind).await.is_err());
        let command = first.wait_for_command(2).await.pop().unwrap();
        assert_eq!(workspace_kind(&command), Some(kind));
        // The host performed the release and recorded its success, but the
        // connection was lost before that result reached the hub.
        let result = ok(&command, Some(workspace(&command, json!({}))));
        let journal_id = first.journal.clone();
        first.disconnect();
        let second = fx
            .spoke(Box::new(|_| None), Some(Cursor { journal_id, through: 2 }), 1)
            .await;
        let waiting = {
            let adapter = Arc::clone(&adapter);
            tokio::spawn(async move { release(&adapter, kind).await })
        };
        second.answer_late(&command, result).await;
        waiting.await.unwrap().unwrap();
        assert!(
            fx.db
                .spoke_assignment_for_execution("exec_1")
                .unwrap()
                .unwrap()
                .resolved
        );
        assert!(second.commands().is_empty());
        // A settled retry also works without a stream.
        second.disconnect();
        release(&adapter, kind).await.unwrap();
    }
}

async fn release(adapter: &Arc<dyn HostAdapter>, kind: Kind) -> anyhow::Result<()> {
    match kind {
        Kind::Release => adapter.release_workspace("lease-1").await,
        Kind::ReleaseWithReset => adapter.release_workspace_with_reset("lease-1").await,
        Kind::ForceRelease => adapter.force_release_lease("lease-1", Some("operator fenced")).await,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn unanswered_sweep_queries_reuse_their_commands() {
    let fx = Fixture::with(short(200)).await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx.spoke(Box::new(standard), None, 0).await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();
    for _ in 0..3 {
        assert!(adapter.probe_remote_worker_alive(4242).await.is_err());
        assert!(adapter.heartbeat_lease("lease-1", None).await.is_err());
    }
    let commands = peer.commands();
    assert_eq!(
        commands
            .iter()
            .filter(|c| matches!(c.body, Some(command::Body::Adopt(_))))
            .count(),
        1
    );
    assert_eq!(
        commands
            .iter()
            .filter(|c| workspace_kind(c) == Some(Kind::Status))
            .count(),
        1
    );
}

#[tokio::test]
async fn full_outbox_refuses_stop_and_does_not_pin_an_unsent_lease() {
    let fx = Fixture::with(short(500)).await;
    fx.ready(HOST);
    fx.ready("other");
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let _peer = fx.spoke(Box::new(standard), None, 0).await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();
    // Fill the durable quota with unanswered host-scoped requests.
    for n in 0..boss_work_db::MAX_UNSETTLED_HOST_COMMANDS {
        let command = Command {
            command_id: format!("pending-{n}"),
            not_after_unix_ms: 4_102_444_800_000,
            body: Some(command::Body::Workspace(WorkspaceOp {
                kind: Kind::ListRepos.into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        fx.db
            .append_host_command(
                HOST,
                &command.command_id,
                None,
                boss_work_db::RunAdmission::Recovery,
                &command.encode_to_vec(),
                &|sequence| {
                    let mut c = command.clone();
                    c.sequence = sequence;
                    c.encode_to_vec()
                },
            )
            .unwrap();
    }
    assert!(matches!(
        adapter.stop_worker("exec_1", Duration::from_secs(1)).await.unwrap(),
        ControlOutcome::NotSubmitted(_)
    ));
    let fresh = fx.adapter(HOST, "exec_2").await.unwrap();
    let error = fresh
        .lease_workspace("repo", "task", None, false, &[])
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<HostCallError>(),
        Some(HostCallError::NotSent { .. })
    ));
    let in_doubt: bool = fx
        .db
        .connect()
        .unwrap()
        .query_row(
            "SELECT in_doubt FROM spoke_assignments WHERE execution_id = 'exec_2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!in_doubt);
    assert!(fx.adapter("other", "exec_2").await.is_ok());
}

#[tokio::test]
async fn settled_uncertain_release_uses_registry_evidence() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx
        .spoke(
            Box::new(|c| match workspace_kind(c) {
                Some(Kind::Release) => Some(refused(c, ResultCode::OutcomeUncertain, "cube reply lost")),
                Some(Kind::ListWorkspaces) => Some(ok(c, Some(workspace(c, json!([]))))),
                _ => standard(c),
            }),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    adapter.release_workspace("lease-1").await.unwrap();
    assert!(
        fx.db
            .spoke_assignment_for_execution("exec_1")
            .unwrap()
            .unwrap()
            .resolved
    );
    assert_eq!(
        peer.commands().iter().filter_map(workspace_kind).collect::<Vec<_>>(),
        [Kind::Lease, Kind::Release, Kind::ListWorkspaces]
    );
}
