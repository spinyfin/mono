//! Retry safety, ownership and routing against a real stream: the lease and stop
//! commands reuse their stored form, every destination refuses an execution
//! another host holds, operations the spoke has no wire form for reach the
//! legacy transport, and recovery queries cross the wire as typed operations.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use boss_engine_recovery::execution_bookmark::{
    ExecutionBookmark, HostRecoveryRequest, HostVoucher, PreservedTip, Voucher,
};
use boss_engine_structured_output::StructuredOutputKind;
use boss_execution_contracts::{CollectOutcome, ControlOutcome, Occupancy};
use boss_protocol_spoke::v1::{Command, PaneLife, WorkspaceKind as Kind, command};
use boss_work_db::HostTransport;
use serde_json::json;

use crate::support::*;
use crate::{PATH, lease, short, spawn, standard, workspace_kind};

fn lease_commands(commands: &[Command]) -> Vec<Command> {
    commands
        .iter()
        .filter(|c| workspace_kind(c) == Some(Kind::Lease))
        .cloned()
        .collect()
}

#[tokio::test]
async fn a_lease_retry_resends_the_stored_command_before_and_after_it_settles() {
    let fx = Fixture::with(short(400)).await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    // The spoke takes the lease and says nothing.
    let peer = fx
        .spoke(
            Box::new(|c| (workspace_kind(c) != Some(Kind::Lease)).then(|| standard(c)).flatten()),
            None,
            0,
        )
        .await;

    let first = adapter
        .lease_workspace("repo", "task", None, false, &[])
        .await
        .unwrap_err();
    assert!(format!("{first:#}").contains("uncertain"), "{first:#}");

    // Time passes, so a freshly built command would carry a later expiry and be
    // refused as a different request under the same id.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let retrying = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.lease_workspace("repo", "task", None, false, &[]).await })
    };
    let stored = peer.wait_for_command(1).await.remove(0);
    peer.answer_late(&stored, ok(&stored, Some(workspace(&stored, lease_json()))))
        .await;
    let granted = retrying.await.unwrap().expect("the late result completes the retry");
    assert_eq!(granted.lease_id, "lease-1");

    // After settlement the same request is answered from the stored result and
    // nothing new leaves the hub.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let again = adapter.lease_workspace("repo", "task", None, false, &[]).await.unwrap();
    assert_eq!(again.lease_id, "lease-1");
    let sent = lease_commands(&peer.commands());
    assert_eq!(sent.len(), 1, "one lease command in total: {sent:?}");
    let wire = fx.db.host_command_wire(HOST, &stored.command_id).unwrap().unwrap();
    assert_eq!(
        prost::Message::decode(wire.as_slice()).map(|c: Command| c.not_after_unix_ms),
        Ok(stored.not_after_unix_ms),
        "the original expiry survives every retry"
    );
}

#[tokio::test]
async fn repeating_a_stop_reuses_durable_state() {
    let fx = Fixture::with(short(400)).await;
    fx.ready(HOST);
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
    spawn(&adapter).await.unwrap();

    // The first stop is accepted and never answered in time.
    let grace = Duration::from_secs(2);
    assert!(matches!(
        adapter.stop_worker("exec_1", grace).await.unwrap(),
        ControlOutcome::Uncertain(_)
    ));
    let retrying = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.stop_worker("exec_1", grace).await })
    };
    let stop = peer
        .wait_for_command(3)
        .await
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Stop(_))))
        .expect("the spoke saw the stop");
    peer.answer_late(&stop, ok(&stop, Some(pane(PaneLife::Exited)))).await;
    assert!(matches!(retrying.await.unwrap().unwrap(), ControlOutcome::Delivered(_)));

    // A repeat after success is answered from the stored result.
    assert!(matches!(
        adapter.stop_worker("exec_1", grace).await.unwrap(),
        ControlOutcome::Delivered(_)
    ));
    let stops = peer
        .commands()
        .iter()
        .filter(|c| matches!(c.body, Some(command::Body::Stop(_))))
        .count();
    assert_eq!(stops, 1, "every attempt reused the one stored stop");
}

#[tokio::test]
async fn a_refused_stop_is_asked_again_as_a_new_request() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let mut refusals = 1;
    let peer = fx
        .spoke(
            Box::new(move |c| match &c.body {
                Some(command::Body::Stop(_)) if refusals > 0 => {
                    refusals -= 1;
                    Some(refused(c, boss_protocol_spoke::v1::ResultCode::Rejected, "not yet"))
                }
                Some(command::Body::Stop(_)) => Some(ok(c, Some(pane(PaneLife::Exited)))),
                _ => standard(c),
            }),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();
    let grace = Duration::from_secs(1);
    assert!(matches!(
        adapter.stop_worker("exec_1", grace).await.unwrap(),
        ControlOutcome::Rejected { .. }
    ));
    assert!(matches!(
        adapter.stop_worker("exec_1", grace).await.unwrap(),
        ControlOutcome::Delivered(_)
    ));
    let ids: Vec<_> = peer
        .commands()
        .into_iter()
        .filter(|c| matches!(c.body, Some(command::Body::Stop(_))))
        .map(|c| c.command_id)
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test]
async fn a_resolved_assignment_admits_no_new_work_but_stays_open_to_recovery() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx.spoke(Box::new(standard), None, 0).await;
    lease(&adapter).await;
    adapter.release_workspace("lease-1").await.unwrap();
    let held = fx.db.spoke_assignment_for_execution("exec_1").unwrap().unwrap();
    assert!(held.resolved);

    let run = boss_protocol_spoke::v1::Run {
        execution_id: held.execution_id.clone(),
        run_id: held.run_id.clone(),
        assignment_generation: held.assignment_generation,
        spawn_token: held.spawn_token.clone(),
    };
    let command_for = |id: &str, body: command::Body| Command {
        command_id: id.into(),
        run: Some(run.clone()),
        not_after_unix_ms: 4_102_444_800_000,
        body: Some(body),
        ..Default::default()
    };
    // Lease and spawn are fenced in the database, in the append transaction,
    // so a resolution that lands after the adapter's own check still wins.
    let new_lease = command_for(
        "late-lease",
        command::Body::Workspace(boss_protocol_spoke::v1::WorkspaceOp {
            kind: Kind::Lease.into(),
            repo_id: "repo".into(),
            ..Default::default()
        }),
    );
    let spawn_again = command_for("late-spawn", command::Body::Spawn(Default::default()));
    for refused in [new_lease, spawn_again] {
        let id = refused.command_id.clone();
        assert!(
            fx.manager
                .command(HOST, refused, Duration::from_millis(300))
                .await
                .is_err()
        );
        assert!(fx.db.host_command_wire(HOST, &id).unwrap().is_none(), "{id} was stored");
    }
    // Recovery still reaches the resolved attempt.
    let diff = command_for(
        "recovery-diff",
        command::Body::Workspace(boss_protocol_spoke::v1::WorkspaceOp {
            kind: Kind::DiffBookmark.into(),
            repo_id: "repo".into(),
            ..Default::default()
        }),
    );
    let _ = fx.manager.command(HOST, diff, Duration::from_millis(300)).await;
    assert!(fx.db.host_command_wire(HOST, "recovery-diff").unwrap().is_some());
    assert!(peer.commands().iter().all(|c| c.command_id != "late-lease"));
}

#[tokio::test]
async fn every_destination_refuses_an_execution_another_host_holds() {
    let fx = Fixture::new().await;
    fx.serve_legacy();
    fx.ready(HOST);
    fx.adapter(HOST, "exec_1").await.unwrap();
    fx.db.bind_spoke_repo("exec_1", 1, "repo").unwrap();
    fx.db.bind_spoke_lease("exec_1", 1, "worker", "lease-1", PATH).unwrap();

    // `legacy_ssh` is a host with no transport row; `pending` and `ready` are
    // enrolled hosts; `local` is the hub's own machine.
    fx.db
        .set_host_transport("pending", HostTransport::SpokePending)
        .unwrap();
    fx.ready("ready");
    for destination in ["local", "ssh-host", "pending", "ready"] {
        let refused = fx
            .adapter(destination, "exec_1")
            .await
            .err()
            .unwrap_or_else(|| panic!("{destination} accepted an execution {HOST} holds"));
        assert!(
            format!("{refused:#}").contains(&format!("still assigned to host {HOST}")),
            "{destination}: {refused:#}"
        );
    }
    assert!(fx.legacy_calls().is_empty());
    // The holder itself still gets its run back, and a resolution frees the rest.
    assert_eq!(fx.adapter(HOST, "exec_1").await.unwrap().host_id(), HOST);
    fx.db.resolve_spoke_assignment("exec_1", 1, "operator_fenced").unwrap();
    for destination in ["local", "ssh-host", "pending", "ready"] {
        assert_eq!(fx.adapter(destination, "exec_1").await.unwrap().host_id(), destination);
    }
}

#[tokio::test]
async fn operations_without_a_wire_form_follow_the_run_that_owns_them() {
    let fx = Fixture::with(short(500)).await;
    fx.serve_legacy();
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let unbound = fx.provider_adapter(HOST).await;
    let peer = fx
        .spoke(
            Box::new(|c| match &c.body {
                Some(command::Body::Adopt(_)) => Some(ok(c, Some(pane(PaneLife::Alive)))),
                _ => standard(c),
            }),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();

    // Anything no spoke assignment names is the legacy transport's to answer.
    assert_eq!(unbound.probe_remote_worker_alive(777).await.unwrap(), Some(true));
    assert_eq!(
        unbound.probe_initial_input_alive("elsewhere", "run-9").await.unwrap(),
        Some(false)
    );
    unbound.cleanup_initial_input("elsewhere", "run-9").await.unwrap();
    assert!(unbound.reattach_events_forward("exec_other", "sock").await.unwrap());
    assert_eq!(
        unbound.read_worker_log_tail("/other/ws", 10).await.unwrap().as_deref(),
        Some("legacy log")
    );
    assert_eq!(
        unbound.probe_workspace_occupancy(Path::new("/other/ws")).await.unwrap(),
        Occupancy::Occupied
    );
    let kind = StructuredOutputKind::PrUrl;
    assert!(matches!(
        unbound
            .collect_structured_output("exec_other", kind, Path::new("/tmp/out"))
            .await
            .unwrap(),
        CollectOutcome::Collected
    ));
    let other = ExecutionBookmark::builder()
        .execution_id("exec_other")
        .repo_path("/legacy/repo".into())
        .host_id(HOST)
        .build();
    let tip = PreservedTip {
        change_id: "k".repeat(32),
        commit_id: "a".repeat(40),
        count: 1,
    };
    assert!(unbound.preserved_work_unpublished(&other, &tip).await.unwrap());
    unbound.prune_execution_bookmark(&other, &[], "main").await.unwrap();
    assert_eq!(
        fx.legacy_calls(),
        [
            "probe_remote_worker_alive:777",
            "probe_initial_input_alive:run-9",
            "cleanup_initial_input:run-9",
            "reattach_events_forward:exec_other",
            "read_worker_log_tail",
            "probe_workspace_occupancy",
            "collect_structured_output:exec_other",
            "preserved_work_unpublished:exec_other",
            "prune_execution_bookmark:exec_other",
        ]
    );

    // The spoke's own run is never sent down the legacy path.
    let before = fx.legacy_calls().len();
    assert_eq!(unbound.probe_remote_worker_alive(4242).await.unwrap(), Some(true));
    assert!(!unbound.reattach_events_forward("exec_1", "sock").await.unwrap());
    assert_eq!(
        unbound.probe_workspace_occupancy(Path::new(PATH)).await.unwrap(),
        Occupancy::Unknown
    );
    assert!(unbound.read_worker_log_tail(PATH, 10).await.unwrap().is_none());
    assert!(matches!(
        unbound
            .collect_structured_output("exec_1", kind, Path::new("/tmp/out"))
            .await
            .unwrap(),
        CollectOutcome::NotAvailable
    ));
    assert_eq!(unbound.probe_initial_input_alive(PATH, "exec_1").await.unwrap(), None);
    unbound.cleanup_initial_input(PATH, "exec_1").await.unwrap();
    assert_eq!(
        fx.legacy_calls().len(),
        before,
        "a spoke run reached the legacy transport"
    );
    assert!(
        peer.commands()
            .iter()
            .any(|c| matches!(c.body, Some(command::Body::Adopt(_)))),
        "the spoke run's liveness was read from its session"
    );
}

#[tokio::test]
async fn preserved_work_check_and_prune_cross_the_wire_as_typed_operations() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx
        .spoke(
            Box::new(|c| {
                let value = match workspace_kind(c)? {
                    Kind::CheckPreservedUnpublished => workspace(c, json!(true)),
                    Kind::PruneBookmark => workspace(
                        c,
                        json!({"Published": {"vouched_by": "PR #1 head", "commit": "b", "forgotten": ["boss/exec_1"], "rewritten": 0}}),
                    ),
                    _ => return standard(c),
                };
                Some(ok(c, Some(value)))
            }),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    adapter.release_workspace("lease-1").await.unwrap();
    let record = ExecutionBookmark::builder()
        .execution_id("exec_1")
        .repo_path("/hosts/workspaces/worker".into())
        .host_id(HOST)
        .build();
    let tip = PreservedTip {
        change_id: "k".repeat(32),
        commit_id: "a".repeat(40),
        count: 2,
    };
    assert!(adapter.preserved_work_unpublished(&record, &tip).await.unwrap());
    let vouchers = [
        Voucher::commit("PR #1 head", &"b".repeat(40)).unwrap(),
        Voucher::remote_bookmark("boss/exec_1", "origin"),
    ];
    let outcome = adapter
        .prune_execution_bookmark(&record, &vouchers, "main")
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        boss_engine_recovery::execution_bookmark::PruneOutcome::Published { .. }
    ));

    let sent = peer.commands();
    let op_of = |kind: Kind| {
        sent.iter()
            .find_map(|c| match &c.body {
                Some(command::Body::Workspace(op)) if op.kind == kind as i32 => Some(op.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {kind:?} command was sent"))
    };
    // Both are store operations, valid after the lease is gone.
    let check = op_of(Kind::CheckPreservedUnpublished);
    assert!(check.workspace_id.is_empty() && check.lease_id.is_empty());
    let request: HostRecoveryRequest = serde_json::from_slice(&check.request_json).unwrap();
    assert_eq!(request.recorded, Some(tip));
    let prune = op_of(Kind::PruneBookmark);
    assert_eq!(prune.trunk, "main");
    let request: HostRecoveryRequest = serde_json::from_slice(&prune.request_json).unwrap();
    assert_eq!(
        request.vouchers,
        [
            HostVoucher::Commit {
                label: "PR #1 head".into(),
                commit_id: "b".repeat(40)
            },
            HostVoucher::RemoteBookmark {
                name: "boss/exec_1".into(),
                remote: "origin".into()
            },
        ]
    );
}

#[tokio::test]
async fn host_scoped_requests_follow_the_spoke_while_it_holds_runs() {
    let fx = Fixture::new().await;
    fx.serve_legacy();
    fx.ready(HOST);
    let unbound = fx.provider_adapter(HOST).await;
    let peer = fx
        .spoke(
            Box::new(|c| match workspace_kind(c) {
                Some(Kind::EnsureRepo) => Some(ok(c, Some(command_result_repo(c, "repo-spoke")))),
                _ => standard(c),
            }),
            None,
            0,
        )
        .await;
    assert_eq!(unbound.ensure_repo("origin").await.unwrap().repo_id, "repo-spoke");

    // The transport falls back to pending while a run still holds a lease on the
    // spoke: its workspaces exist only there, so host-scoped access stays put.
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    lease(&adapter).await;
    fx.db.set_host_transport(HOST, HostTransport::SpokePending).unwrap();
    assert_eq!(unbound.ensure_repo("origin").await.unwrap().repo_id, "repo-spoke");
    assert!(fx.legacy_calls().is_empty());

    // Once nothing is held, a host not taking spoke work is answered by SSH.
    adapter.release_workspace("lease-1").await.unwrap();
    assert!(unbound.ensure_repo("origin").await.is_err());
    assert_eq!(fx.legacy_calls(), ["ensure_repo"]);
    let ensures = peer
        .commands()
        .iter()
        .filter(|c| workspace_kind(c) == Some(Kind::EnsureRepo))
        .count();
    assert_eq!(ensures, 2);
}

fn command_result_repo(c: &Command, repo: &str) -> boss_protocol_spoke::v1::command_result::Value {
    let boss_protocol_spoke::v1::command_result::Value::Workspace(mut result) = workspace(c, json!({})) else {
        unreachable!()
    };
    result.repo_id = repo.into();
    boss_protocol_spoke::v1::command_result::Value::Workspace(result)
}
