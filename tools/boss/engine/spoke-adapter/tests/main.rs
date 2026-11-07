//! The hub adapter against a real hub: mTLS gRPC stream manager, work database
//! outbox, and a scripted spoke on the other end of the stream.
mod routing;
mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, HostRecoveryRequest, WorkspaceTipSource};
use boss_execution_contracts::{
    ControlOutcome, HostAdapter, HostAdapterProvider, RunWaitState, SpawnOutcomeUncertain, WorkerInputKind, WorkerLife,
};
use boss_protocol_spoke::v1::{
    Bookmark, Command, CommandResult, DeliveryKind, PaneLife, ResultCode, WorkspaceKind as Kind, command,
    command_result,
};
use boss_spoke_adapter::{HostCallError, SpokeAdapterConfig};
use serde_json::json;
use support::*;

const PATH: &str = "/hosts/workspaces/worker";

fn workspace_kind(command: &Command) -> Option<Kind> {
    match &command.body {
        Some(command::Body::Workspace(op)) => Kind::try_from(op.kind).ok(),
        _ => None,
    }
}

/// The answers a healthy spoke gives to lease, release and spawn.
fn standard(command: &Command) -> Option<CommandResult> {
    match (&command.body, workspace_kind(command)) {
        (_, Some(Kind::Lease)) => Some(ok(command, Some(workspace(command, lease_json())))),
        (_, Some(Kind::Release | Kind::ForceRelease)) => Some(ok(command, Some(workspace(command, json!({}))))),
        (Some(command::Body::Spawn(_)), _) => Some(ok(command, Some(pane(PaneLife::Alive)))),
        _ => None,
    }
}

fn short(deadline_ms: u64) -> SpokeAdapterConfig {
    SpokeAdapterConfig::builder()
        .command_deadline(Duration::from_millis(deadline_ms))
        .spawn_deadline(Duration::from_millis(deadline_ms))
        .build()
}

async fn lease(adapter: &Arc<dyn HostAdapter>) {
    let lease = adapter.lease_workspace("repo", "task", None, false, &[]).await.unwrap();
    assert_eq!(lease.lease_id, "lease-1");
}

async fn spawn(adapter: &Arc<dyn HostAdapter>) -> anyhow::Result<boss_execution_contracts::RunOutcome> {
    adapter
        .spawn_worker("worker-1", &execution("exec_1"), &work_item(), Path::new(PATH), None)
        .await
}

#[tokio::test]
async fn ownership_is_recorded_before_the_first_command_leaves() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();

    // The assignment is durable and nothing has been stored for the wire yet.
    let reserved = fx.db.spoke_assignment_for_execution("exec_1").unwrap().unwrap();
    assert_eq!(reserved.host_id, HOST);
    assert!(fx.db.host_command_after(HOST, 0).unwrap().is_none());

    let peer = fx.spoke(Box::new(standard), None, 0).await;
    lease(&adapter).await;

    let sent = peer.wait_for_command(1).await;
    let run = sent[0].run.clone().expect("a lease names its run");
    assert_eq!(
        (run.execution_id, run.run_id, run.assignment_generation, run.spawn_token),
        (
            reserved.execution_id.clone(),
            reserved.run_id.clone(),
            reserved.assignment_generation,
            reserved.spawn_token.clone()
        )
    );
    let held = fx.db.spoke_assignment_for_execution("exec_1").unwrap().unwrap();
    assert_eq!(held.repo_id.as_deref(), Some("repo"));
    assert_eq!(held.lease_id.as_deref(), Some("lease-1"));
    assert_eq!(held.workspace_id.as_deref(), Some("worker"));
}

#[tokio::test]
async fn an_unresolved_assignment_is_never_given_to_a_second_host() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    fx.ready("other");
    // A reservation that never leased anything does not pin the execution.
    fx.adapter(HOST, "exec_1").await.unwrap();
    let moved = fx.adapter("other", "exec_1").await.unwrap();
    assert_eq!(moved.host_id(), "other");

    // Once a lease is held, the host that holds it owns the execution.
    let adapter = fx.adapter("other", "exec_2").await.unwrap();
    fx.db.bind_spoke_repo("exec_2", 1, "repo").unwrap();
    fx.db.bind_spoke_lease("exec_2", 1, "worker", "lease-2", PATH).unwrap();
    let refused = fx
        .adapter(HOST, "exec_2")
        .await
        .err()
        .expect("a second host is refused");
    assert!(
        format!("{refused:#}").contains("still assigned to host other"),
        "{refused:#}"
    );
    drop(adapter);
    fx.db.resolve_spoke_assignment("exec_2", 1, "operator_fenced").unwrap();
    assert_eq!(fx.adapter(HOST, "exec_2").await.unwrap().host_id(), HOST);
}

#[tokio::test]
async fn a_command_naming_a_run_the_hub_does_not_hold_is_never_stored_or_sent() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx.spoke(Box::new(standard), None, 0).await;
    let mut forged = peer_command("forged");
    forged.run.as_mut().unwrap().spawn_token = "not-the-token".into();
    let outcome = fx.manager.command(HOST, forged, Duration::from_secs(2)).await;
    assert_eq!(outcome, Err(boss_spoke_stream::CommandOutcome::OutcomeUncertain));
    assert!(fx.db.host_command_wire(HOST, "forged").unwrap().is_none());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(peer.commands().is_empty());

    // The held run, by contrast, is accepted.
    let genuine = peer_command("genuine");
    let held = fx.db.spoke_assignment_for_execution("exec_1").unwrap().unwrap();
    let mut genuine = genuine;
    genuine.run.as_mut().unwrap().spawn_token = held.spawn_token;
    genuine.run.as_mut().unwrap().run_id = held.run_id;
    let stored = fx.manager.command(HOST, genuine, Duration::from_millis(300)).await;
    assert_eq!(
        stored,
        Err(boss_spoke_stream::CommandOutcome::OutcomeUncertain),
        "no answer yet"
    );
    assert!(fx.db.host_command_wire(HOST, "genuine").unwrap().is_some());
}

fn peer_command(id: &str) -> Command {
    Command {
        command_id: id.into(),
        run: Some(boss_protocol_spoke::v1::Run {
            execution_id: "exec_1".into(),
            run_id: "exec_1-g1".into(),
            assignment_generation: 1,
            spawn_token: "token".into(),
        }),
        not_after_unix_ms: 4_102_444_800_000,
        body: Some(command::Body::Stop(boss_protocol_spoke::v1::Stop { grace_ms: 1 })),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_spawn_timeout_is_uncertain_and_a_retry_never_starts_a_second_worker() {
    let fx = Fixture::with(short(500)).await;
    fx.ready(HOST);
    fx.ready("other");
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    // The spoke takes the spawn and goes quiet.
    let peer = fx.spoke(Box::new(standard_without_spawn), None, 0).await;
    lease(&adapter).await;

    let error = spawn(&adapter).await.unwrap_err();
    assert!(error.downcast_ref::<SpawnOutcomeUncertain>().is_some(), "{error:#}");

    // The execution stays with this host, and its workspace is not released
    // under a worker that may be running.
    assert!(fx.adapter("other", "exec_1").await.is_err());
    let refused = adapter.release_workspace("lease-1").await.unwrap_err();
    assert!(
        format!("{refused:#}").contains("may have started a worker"),
        "{refused:#}"
    );

    // Dispatching again reaches the same assignment and the same stored spawn.
    let retry = fx.adapter(HOST, "exec_1").await.unwrap();
    let waiting = tokio::spawn(async move { spawn(&retry).await });
    let spawn_command = peer
        .wait_for_command(2)
        .await
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        .expect("the spoke saw the spawn");
    peer.answer_late(&spawn_command, ok(&spawn_command, Some(pane(PaneLife::Alive))))
        .await;
    let outcome = waiting.await.unwrap().expect("the late result completes the retry");
    assert_eq!(outcome.wait_state, RunWaitState::WorkerPaneAlive);

    let spawns = peer
        .commands()
        .iter()
        .filter(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        .count();
    assert_eq!(spawns, 1, "the retry reused the stored command");
    assert_eq!(
        fx.db
            .open_spoke_assignment_for_pane_pid(HOST, 4242)
            .unwrap()
            .map(|a| a.execution_id),
        Some("exec_1".into())
    );
}

fn standard_without_spawn(command: &Command) -> Option<CommandResult> {
    if matches!(command.body, Some(command::Body::Spawn(_))) {
        None
    } else {
        standard(command)
    }
}

#[tokio::test]
async fn a_result_that_arrives_after_a_reconnect_answers_the_stored_spawn() {
    let fx = Fixture::with(short(500)).await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let first = fx.spoke(Box::new(standard_without_spawn), None, 0).await;
    lease(&adapter).await;
    assert!(
        spawn(&adapter)
            .await
            .unwrap_err()
            .downcast_ref::<SpawnOutcomeUncertain>()
            .is_some()
    );
    let spawn_command = first
        .commands()
        .into_iter()
        .find(|c| matches!(c.body, Some(command::Body::Spawn(_))))
        .unwrap();
    let journal = first.journal.clone();
    first.disconnect();

    // The spoke reconnects with its command cursor, so the hub does not resend
    // the spawn, and replays the result it produced meanwhile.
    let second = fx
        .spoke(
            Box::new(|_| None),
            Some(boss_protocol_spoke::v1::Cursor {
                journal_id: journal,
                through: 2,
            }),
            1,
        )
        .await;
    let waiting = {
        let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
        tokio::spawn(async move { spawn(&adapter).await })
    };
    second
        .answer_late(&spawn_command, ok(&spawn_command, Some(pane(PaneLife::Alive))))
        .await;
    waiting.await.unwrap().expect("the replayed result answers the retry");
    assert!(second.commands().is_empty(), "a receipted command is not resent");
    assert_eq!(
        fx.db
            .host_command_result(HOST, &spawn_command.command_id)
            .unwrap()
            .map(|bytes| {
                use prost::Message;
                CommandResult::decode(bytes.as_slice()).unwrap().code
            }),
        Some(ResultCode::Ok as i32)
    );
}

#[tokio::test]
async fn runs_keep_their_transport_and_a_spoke_lease_is_never_sent_over_ssh() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    let unbound = fx.provider.adapter_for(&fx.host(HOST)).await.unwrap();

    // A lease no spoke assignment holds belongs to the legacy transport.
    let legacy = unbound.release_workspace("old-lease").await.unwrap_err();
    assert!(
        format!("{legacy:#}").contains("legacy transport was used"),
        "{legacy:#}"
    );
    // Acquiring a spoke lease needs an execution-bound adapter; it does not fall back.
    let unowned = unbound
        .lease_workspace("repo", "task", None, false, &[])
        .await
        .unwrap_err();
    assert!(format!("{unowned:#}").contains("execution-bound"), "{unowned:#}");

    // A lease a spoke assignment holds is released over the stream, never legacy.
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let peer = fx.spoke(Box::new(standard), None, 0).await;
    lease(&adapter).await;
    unbound.release_workspace("lease-1").await.unwrap();
    let kinds: Vec<_> = peer.commands().iter().filter_map(workspace_kind).collect();
    assert_eq!(kinds, [Kind::Lease, Kind::Release]);
    assert!(
        fx.db
            .spoke_assignment_for_execution("exec_1")
            .unwrap()
            .unwrap()
            .resolved
    );

    // New work only goes to the spoke once the host is ready, and never for `local`.
    fx.db
        .set_host_transport(HOST, boss_work_db::HostTransport::SpokePending)
        .unwrap();
    assert!(format!("{:#}", fx.adapter(HOST, "exec_9").await.err().unwrap()).contains("legacy transport was used"));
    assert!(fx.db.spoke_assignment_for_execution("exec_9").unwrap().is_none());
    fx.db
        .set_host_transport("local", boss_work_db::HostTransport::SpokeReady)
        .unwrap();
    assert!(format!("{:#}", fx.adapter("local", "exec_9").await.err().unwrap()).contains("legacy transport was used"));
}

#[tokio::test]
async fn control_requests_name_the_run_and_keep_receipt_delivery_and_uncertainty_apart() {
    let fx = Fixture::with(short(600)).await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let gone = Arc::new(AtomicBool::new(false));
    let script_gone = Arc::clone(&gone);
    let peer = fx
        .spoke(
            Box::new(move |c| match &c.body {
                Some(command::Body::Deliver(d)) if d.kind == DeliveryKind::Interrupt as i32 => Some(refused(
                    c,
                    ResultCode::Rejected,
                    "worker is not running; input not sent",
                )),
                // A probe is accepted by the spoke but never answered.
                Some(command::Body::Deliver(d)) if d.kind == DeliveryKind::Probe as i32 => None,
                Some(command::Body::Deliver(_) | command::Body::Stop(_)) => Some(ok(c, Some(pane(PaneLife::Alive)))),
                Some(command::Body::Adopt(_)) => Some(ok(
                    c,
                    Some(pane(if script_gone.load(Ordering::SeqCst) {
                        PaneLife::Missing
                    } else {
                        PaneLife::Alive
                    })),
                )),
                _ => standard(c),
            }),
            None,
            0,
        )
        .await;
    // Nothing is controllable before the worker exists.
    assert!(matches!(
        adapter
            .deliver_worker_input("nobody", WorkerInputKind::Nudge, "hi")
            .await
            .unwrap(),
        ControlOutcome::NotSubmitted(_)
    ));
    lease(&adapter).await;
    spawn(&adapter).await.unwrap();
    let held = fx.db.spoke_assignment_for_execution("exec_1").unwrap().unwrap();

    let ControlOutcome::Delivered(pane) = adapter
        .deliver_worker_input("exec_1", WorkerInputKind::Nudge, "hi")
        .await
        .unwrap()
    else {
        panic!("delivered")
    };
    assert_eq!(pane.life, WorkerLife::Alive);
    assert!(matches!(
        adapter.interrupt_worker("exec_1").await.unwrap(),
        ControlOutcome::Rejected { ref reason, .. } if reason.contains("not running")
    ));
    assert!(matches!(
        adapter
            .deliver_worker_input("exec_1", WorkerInputKind::Probe, "p")
            .await
            .unwrap(),
        ControlOutcome::Uncertain(_)
    ));
    assert!(matches!(
        adapter.stop_worker("exec_1", Duration::from_secs(5)).await.unwrap(),
        ControlOutcome::Delivered(_)
    ));

    let seen = peer.commands();
    let delivery = seen.iter().find_map(|c| match &c.body {
        Some(command::Body::Deliver(d)) if d.kind == DeliveryKind::Nudge as i32 => Some((c, d)),
        _ => None,
    });
    let (command, deliver) = delivery.expect("the nudge reached the spoke");
    assert_eq!(deliver.input_utf8, b"hi");
    assert_eq!(command.run.as_ref().unwrap().spawn_token, held.spawn_token);
    let stop = seen.iter().find_map(|c| match &c.body {
        Some(command::Body::Stop(stop)) => Some(stop.grace_ms),
        _ => None,
    });
    assert_eq!(stop, Some(5000));

    // Liveness is read from the token's session, keyed by the recorded pid.
    assert_eq!(adapter.probe_remote_worker_alive(4242).await.unwrap(), Some(true));
    gone.store(true, Ordering::SeqCst);
    assert_eq!(adapter.probe_remote_worker_alive(4242).await.unwrap(), Some(false));
    assert_eq!(
        adapter.probe_remote_worker_alive(1).await.unwrap(),
        None,
        "an unknown pid proves nothing"
    );
    assert_eq!(
        adapter.observe_worker("exec_1").await.unwrap().unwrap().life,
        WorkerLife::Missing
    );

    // With the stream gone nothing is sent, and the answer says so.
    peer.disconnect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(matches!(
        adapter.interrupt_worker("exec_1").await.unwrap(),
        ControlOutcome::NotSubmitted(_)
    ));
    assert!(
        adapter.probe_remote_worker_alive(4242).await.is_err(),
        "an unreachable host is not a dead worker"
    );
}

#[tokio::test]
async fn recovery_operations_are_typed_workspace_commands() {
    let fx = Fixture::new().await;
    fx.ready(HOST);
    let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
    let commit = "a".repeat(40);
    let script_commit = commit.clone();
    let peer = fx
        .spoke(
            Box::new(move |c| {
                let value = match workspace_kind(c)? {
                    Kind::PositionedParent | Kind::StartFresh => workspace(c, json!(script_commit)),
                    Kind::CreateBookmark => {
                        let Some(command_result::Value::Workspace(mut created)) = Some(workspace(c, json!({}))) else {
                            unreachable!()
                        };
                        created.bookmark = Some(Bookmark {
                            repo_id: "repo".into(),
                            execution_id: "exec_1".into(),
                            head: "boss/exec_1".into(),
                            recovery: "boss-recovery/exec_1".into(),
                            base: "boss-base/exec_1".into(),
                            anchor_commit: script_commit.clone(),
                            pr_head_before: String::new(),
                        });
                        command_result::Value::Workspace(created)
                    }
                    Kind::ConfirmRecovery => workspace(
                        c,
                        json!({"tip": {"change_id": "c", "commit_id": "d", "count": 2}, "inspect": null}),
                    ),
                    Kind::RestorePreserved => workspace(c, json!(null)),
                    Kind::DiffBookmark => workspace(c, json!("diff --git")),
                    Kind::ReleaseHeads | Kind::ReleaseHeadsConfigured => workspace(c, json!([["boss/exec_1", "abc"]])),
                    _ => return standard(c),
                };
                Some(ok(c, Some(value)))
            }),
            None,
            0,
        )
        .await;
    lease(&adapter).await;
    let path = Path::new(PATH);

    assert_eq!(adapter.positioned_parent(path).await.unwrap(), commit);
    assert_eq!(adapter.start_fresh_workspace(path).await.unwrap(), commit);
    let record = adapter
        .create_execution_bookmark(path, "exec_1", None, None)
        .await
        .unwrap();
    assert_eq!(record.host_id, HOST);
    assert_eq!(record.base_commit.as_deref(), Some(commit.as_str()));
    let outcome = adapter
        .confirm_workspace_recovery_from(&record, path, None, Some(7), WorkspaceTipSource::Live)
        .await
        .unwrap();
    assert_eq!(outcome.tip.unwrap().count, 2);
    assert!(
        adapter
            .restore_preserved_work(&record, path, None)
            .await
            .unwrap()
            .is_none()
    );

    // Recovery that needs no working copy outlives the lease.
    adapter.release_workspace("lease-1").await.unwrap();
    assert_eq!(adapter.execution_bookmark_diff(&record).await.unwrap(), "diff --git");
    assert_eq!(
        adapter
            .release_execution_bookmark_heads(&record, false)
            .await
            .unwrap()
            .len(),
        1
    );

    // A record from another host is refused before anything is sent.
    let foreign = ExecutionBookmark {
        host_id: "elsewhere".into(),
        ..record.clone()
    };
    assert!(adapter.execution_bookmark_diff(&foreign).await.is_err());

    let sent = peer.commands();
    let op_of = |kind: Kind| {
        sent.iter()
            .find_map(|c| match &c.body {
                Some(command::Body::Workspace(op)) if op.kind == kind as i32 => Some(op.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {kind:?} command was sent"))
    };
    let confirm = op_of(Kind::ConfirmRecovery);
    assert_eq!(
        (confirm.workspace_id.as_str(), confirm.lease_id.as_str()),
        ("worker", "lease-1")
    );
    let request: HostRecoveryRequest = serde_json::from_slice(&confirm.request_json).unwrap();
    assert_eq!(
        (request.since_unix, request.tip_source),
        (Some(7), Some(WorkspaceTipSource::Live))
    );
    // A store operation names neither workspace nor lease, only the bookmark.
    let diff = op_of(Kind::DiffBookmark);
    assert!(diff.workspace_id.is_empty() && diff.lease_id.is_empty() && diff.bookmark.is_some());
    // Keeping the publication names needs the configured kind; the original
    // kind carries no payload and an older peer would reject the new one.
    let release: HostRecoveryRequest =
        serde_json::from_slice(&op_of(Kind::ReleaseHeadsConfigured).request_json).unwrap();
    assert_eq!(release.release_names, Some(false));
    assert!(sent.iter().all(|c| !matches!(
        &c.body,
        Some(command::Body::Workspace(op)) if op.kind == Kind::ReleaseHeads as i32
    )));
}

#[test]
fn host_call_errors_say_whether_anything_happened() {
    let uncertain = HostCallError::Uncertain {
        host_id: HOST.into(),
        op: "spawn",
        command_id: "spawn-1".into(),
        reason: "no result".into(),
    };
    assert!(uncertain.to_string().contains("uncertain"));
    let unsent = HostCallError::NotSent {
        host_id: HOST.into(),
        op: "spawn",
        reason: "host unavailable".into(),
    };
    assert!(unsent.to_string().contains("nothing was sent"));
}
