use super::*;
use boss_protocol_spoke::v1::{Cursor, WorkspaceOp};
use prost::Message;

#[tokio::test]
async fn definitive_lease_errors_clear_the_in_doubt_flag() {
    for failure in ["unknown", "parse", "mismatch", "bind", "repo"] {
        let fx = Fixture::new().await;
        fx.ready(HOST);
        let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
        if failure == "repo" {
            let assignment = fx.db.spoke_assignment_for_execution("exec_1").unwrap().unwrap();
            fx.db
                .bind_spoke_repo("exec_1", assignment.assignment_generation, "other")
                .unwrap();
            fx.db
                .set_spoke_in_doubt("exec_1", assignment.assignment_generation, true)
                .unwrap();
        }
        if failure == "bind" {
            let assignment = fx.db.spoke_assignment_for_execution("exec_1").unwrap().unwrap();
            fx.db
                .bind_spoke_lease("exec_1", assignment.assignment_generation, "old", "old-lease", PATH)
                .unwrap();
        }
        let _peer = fx
            .spoke(
                Box::new(move |c| {
                    if workspace_kind(c) != Some(Kind::Lease) {
                        return standard(c);
                    }
                    let value = match failure {
                        "unknown" => return Some(refused(c, ResultCode::Unknown, "unknown lease")),
                        "parse" => workspace(c, json!({})),
                        "mismatch" => {
                            let command_result::Value::Workspace(mut value) = workspace(c, lease_json()) else {
                                unreachable!()
                            };
                            value.lease_id = "different".into();
                            command_result::Value::Workspace(value)
                        }
                        "bind" => workspace(c, lease_json()),
                        _ => unreachable!(),
                    };
                    Some(ok(c, Some(value)))
                }),
                None,
                0,
            )
            .await;
        let error = adapter
            .lease_workspace("repo", "task", None, false, &[])
            .await
            .unwrap_err();
        let expected = match failure {
            "unknown" => "unknown lease",
            "parse" => "missing field",
            "mismatch" => "disagrees with its own result",
            "bind" => "already holds a different lease",
            "repo" => "already leases in another repository",
            _ => unreachable!(),
        };
        assert!(format!("{error:#}").contains(expected), "{failure}: {error:#}");
        assert!(
            error
                .downcast_ref::<boss_execution_contracts::LeaseOutcomeUncertain>()
                .is_none(),
            "{failure}: {error:#}"
        );
        assert!(
            !fx.db.spoke_execution_in_doubt("exec_1").unwrap(),
            "{failure}: {error:#}"
        );
    }
}

#[tokio::test]
async fn reset_release_is_clearly_unsent_to_an_old_peer() {
    use boss_protocol_spoke::negotiate::{Contracts, feature};
    let fx = Fixture::new().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let mut contracts = Contracts::first_release();
    contracts
        .features
        .retain(|f| f.name != feature::WORKSPACE_RESET_RELEASE);
    let peer = fx.spoke_contracts(Box::new(standard), None, 0, contracts).await;
    lease(&adapter).await;
    let error = adapter.release_workspace_with_reset("lease-1").await.unwrap_err();
    assert!(
        matches!(error.downcast_ref::<HostCallError>(), Some(HostCallError::NotSent { reason, .. })
        if reason.contains("workspace-reset-release/1"))
    );
    assert_eq!(peer.commands().len(), 1);
    let count: u32 = fx
        .db
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM host_commands WHERE host_id = ?1", [HOST], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
    adapter.release_workspace("lease-1").await.unwrap();
    assert_eq!(peer.commands().len(), 2);
}

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
    let fx = Fixture::with(short(2_000)).await;
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
async fn answered_sweeps_keep_probe_history_and_lookup_work_bounded() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let _peer = fx
        .spoke(
            Box::new(|c| match (&c.body, workspace_kind(c)) {
                (Some(command::Body::Adopt(_)), _) => Some(ok(c, Some(pane(PaneLife::Alive)))),
                (_, Some(Kind::Status)) => Some(ok(
                    c,
                    Some(workspace(
                        c,
                        json!({
                            "workspace_id": "worker", "workspace_path": PATH, "state": "leased", "lease_id": "lease-1"
                        }),
                    )),
                )),
                _ => standard(c),
            }),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();
    let assignment = fx.db.spoke_assignment_for_execution("exec_1").unwrap().unwrap();
    for sweep in 0..25 {
        assert_eq!(adapter.probe_remote_worker_alive(4242).await.unwrap(), Some(true));
        adapter.heartbeat_lease("lease-1", None).await.unwrap();
        let conn = fx.db.connect().unwrap();
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM host_commands WHERE host_id = ?1", [HOST], |r| {
                r.get(0)
            })
            .unwrap();
        // Lease + spawn, and at most two attempts per observation. The single
        // range lookup in refresh_host_probe therefore examines bounded history.
        assert!(count <= 6, "sweep {sweep}: {count} commands");
    }
    // A restarted hub starts from the retained latest attempt, not attempt zero.
    for prefix in ["adopt", "status"] {
        let base = format!("{prefix}-{}", assignment.run_id);
        assert_eq!(fx.db.refresh_host_probe(HOST, &base).unwrap(), format!("{base}-a25"));
    }
    assert!(
        !fx.db
            .spoke_assignment_for_execution("exec_1")
            .unwrap()
            .unwrap()
            .resolved
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

fn interrupted_spawn(
    adopt: impl Fn(&Command) -> Option<CommandResult> + Send + Sync + 'static,
    stop: impl Fn(&Command) -> Option<CommandResult> + Send + Sync + 'static,
) -> Script {
    Box::new(move |c| match &c.body {
        Some(command::Body::Spawn(_)) => Some(refused(c, ResultCode::OutcomeUncertain, "interrupted launch")),
        Some(command::Body::Adopt(_)) => adopt(c),
        Some(command::Body::Stop(_)) => stop(c),
        _ => standard(c),
    })
}

fn no_stop(_: &Command) -> Option<CommandResult> {
    None
}

#[tokio::test]
async fn an_interrupted_spawn_the_host_then_reports_absent_no_longer_blocks_release() {
    let fx = Fixture::with(short(500)).await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let _peer = fx
        .spoke(
            interrupted_spawn(|c| Some(refused(c, ResultCode::Absent, "no session")), no_stop),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    let error = spawn(&adapter).await.unwrap_err();
    assert!(error.downcast_ref::<SpawnOutcomeUncertain>().is_none(), "{error:#}");
    assert!(format!("{error:#}").contains("was gone"), "{error:#}");
    adapter.release_workspace("lease-1").await.unwrap();
    assert!(
        fx.db
            .spoke_assignment_for_execution("exec_1")
            .unwrap()
            .unwrap()
            .resolved
    );
}

#[tokio::test]
async fn an_adopted_alive_worker_keeps_the_lease_until_its_session_is_gone() {
    let fx = Fixture::with(short(500)).await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let exited = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&exited);
    let _peer = fx
        .spoke(
            interrupted_spawn(
                move |c| {
                    let life = if seen.load(Ordering::SeqCst) {
                        PaneLife::Exited
                    } else {
                        PaneLife::Alive
                    };
                    Some(ok(c, Some(pane(life))))
                },
                no_stop,
            ),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    let outcome = spawn(&adapter).await.expect("an alive adopted worker is launched");
    assert_eq!(outcome.wait_state, RunWaitState::WorkerPaneAlive);
    assert!(
        outcome.result_summary.is_some(),
        "the adopt path reports the same summary"
    );
    assert_eq!(
        fx.db
            .open_spoke_assignment_for_pane_pid(HOST, 4242)
            .unwrap()
            .map(|a| a.execution_id),
        Some("exec_1".into())
    );
    let refused = adapter.release_workspace("lease-1").await.unwrap_err();
    assert!(
        format!("{refused:#}").contains("may have started a worker"),
        "{refused:#}"
    );
    exited.store(true, Ordering::SeqCst);
    adapter.release_workspace("lease-1").await.unwrap();
    assert!(
        fx.db
            .spoke_assignment_for_execution("exec_1")
            .unwrap()
            .unwrap()
            .resolved
    );
}

#[tokio::test]
async fn a_settled_uncertain_spawn_adopted_alive_is_stopped_then_released() {
    let fx = Fixture::with(short(500)).await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let gone = Arc::new(AtomicBool::new(false));
    let adopt_gone = Arc::clone(&gone);
    let stop_gone = Arc::clone(&gone);
    let peer = fx
        .spoke(
            interrupted_spawn(
                move |c| {
                    let life = if adopt_gone.load(Ordering::SeqCst) {
                        PaneLife::Exited
                    } else {
                        PaneLife::Alive
                    };
                    Some(ok(c, Some(pane(life))))
                },
                move |c| {
                    stop_gone.store(true, Ordering::SeqCst);
                    Some(ok(c, Some(pane(PaneLife::Exited))))
                },
            ),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    let outcome = spawn(&adapter).await.expect("an alive adopted worker is launched");
    assert_eq!(outcome.wait_state, RunWaitState::WorkerPaneAlive);
    let refused = adapter.release_workspace("lease-1").await.unwrap_err();
    assert!(
        format!("{refused:#}").contains("may have started a worker"),
        "{refused:#}"
    );
    adapter
        .stop_worker("exec_1", Duration::ZERO)
        .await
        .expect("the confirmed stop is delivered");
    assert!(
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Stop(_)))),
        "Stop is issued rather than flipping Adopt to Exited"
    );
    adapter.release_workspace("lease-1").await.unwrap();
    assert!(
        fx.db
            .spoke_assignment_for_execution("exec_1")
            .unwrap()
            .unwrap()
            .resolved
    );
}

#[tokio::test]
async fn an_adopted_alive_worker_that_never_accepted_its_brief_is_stopped_not_reported_running() {
    let fx = Fixture::with(short(300)).await;
    fx.ready(HOST);
    fx.signals.0.store(false, Ordering::SeqCst);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx
        .spoke(
            interrupted_spawn(
                |c| Some(ok(c, Some(pane(PaneLife::Alive)))),
                |c| Some(ok(c, Some(pane(PaneLife::Exited)))),
            ),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    let error = spawn(&adapter).await.unwrap_err();
    assert!(error.downcast_ref::<SpawnOutcomeUncertain>().is_none(), "{error:#}");
    assert!(
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Stop(_)))),
        "the idle worker is stopped"
    );
}

#[tokio::test]
async fn a_launched_worker_whose_stop_is_not_confirmed_is_settled_uncertain() {
    let fx = Fixture::with(short(300)).await;
    fx.ready(HOST);
    fx.signals.0.store(false, Ordering::SeqCst);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let _peer = fx
        .spoke(
            Box::new(|c| match &c.body {
                Some(command::Body::Stop(_)) => Some(refused(c, ResultCode::OutcomeUncertain, "stop lost")),
                _ => standard(c),
            }),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    let error = spawn(&adapter).await.unwrap_err();
    let uncertain = error
        .downcast_ref::<SpawnOutcomeUncertain>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(
        uncertain.settled,
        "the spawn itself answered; replaying it cannot learn the stop"
    );
}

#[tokio::test]
async fn an_adopt_that_finds_no_session_does_not_end_the_doubt_of_an_unanswered_spawn() {
    let fx = Fixture::with(short(300)).await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let _peer = fx
        .spoke(
            Box::new(|c| match &c.body {
                Some(command::Body::Spawn(_)) => None,
                Some(command::Body::Adopt(_)) => Some(refused(c, ResultCode::Absent, "no session")),
                _ => standard(c),
            }),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    let error = spawn(&adapter).await.unwrap_err();
    assert!(error.downcast_ref::<SpawnOutcomeUncertain>().is_some(), "{error:#}");
    // The stored spawn is still deliverable, so a session that does not exist
    // yet is no evidence that nothing will start.
    let refused = adapter.release_workspace("lease-1").await.unwrap_err();
    assert!(
        format!("{refused:#}").contains("may have started a worker"),
        "{refused:#}"
    );
    assert!(
        !fx.db
            .spoke_assignment_for_execution("exec_1")
            .unwrap()
            .unwrap()
            .resolved
    );
}

#[tokio::test]
async fn a_stop_that_stays_unconfirmed_is_retried_without_waiting_for_the_prompt_again() {
    let fx = Fixture::with(short(300)).await;
    fx.ready(HOST);
    fx.signals.0.store(false, Ordering::SeqCst);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx
        .spoke(
            Box::new(|c| match &c.body {
                Some(command::Body::Stop(_)) => None,
                _ => standard(c),
            }),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    let first = spawn(&adapter).await.unwrap_err();
    let uncertain = first.downcast_ref::<SpawnOutcomeUncertain>().expect("stop unconfirmed");
    assert!(uncertain.settled, "{first:#}");
    // A later attempt asks only about the stop: it waits for the stop's
    // answer only, not for the prompt a second time as well.
    let started = std::time::Instant::now();
    let second = spawn(&adapter).await.unwrap_err();
    assert!(second.downcast_ref::<SpawnOutcomeUncertain>().is_some(), "{second:#}");
    assert!(
        started.elapsed() < Duration::from_millis(450),
        "{:?}",
        started.elapsed()
    );
    let stop = peer
        .commands()
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Stop(_))))
        .expect("a stop was sent");
    peer.answer_late(&stop, ok(&stop, Some(pane(PaneLife::Exited)))).await;
    let third = spawn(&adapter).await.unwrap_err();
    assert!(third.downcast_ref::<SpawnOutcomeUncertain>().is_none(), "{third:#}");
}
