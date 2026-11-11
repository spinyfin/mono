use super::{
    ServerStateOverrides,
    pane_ops::SendInputError,
    probes::ProbeDispatchOutcome,
    tests::{register_idle_worker_with_driver, register_working_worker_with_driver, test_server_state_with_overrides},
    worker_events::{dispatch_probe_now, dispatch_probe_on_post_tool_use},
};
use anyhow::Result;
use async_trait::async_trait;
use boss_execution_contracts::*;
use boss_protocol::{ProbeDeliveryState, WorkExecution, WorkItem};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

struct Remote {
    outcome: ControlOutcome,
    calls: Mutex<Vec<WorkerInputKind>>,
}
struct Provider(Arc<Remote>);

#[async_trait]
impl HostAdapterProvider for Provider {
    async fn adapter_for(&self, _: &boss_work_db::Host) -> Result<Arc<dyn HostAdapter>> {
        Ok(self.0.clone())
    }
}

#[async_trait]
impl HostAdapter for Remote {
    fn host_id(&self) -> &str {
        "spoke"
    }
    fn command_repr(&self, _: &[&str]) -> Option<(String, String)> {
        None
    }
    async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
        unreachable!()
    }
    async fn lease_workspace(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: bool,
        _: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        unreachable!()
    }
    async fn release_workspace(&self, _: &str) -> Result<()> {
        unreachable!()
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
        unreachable!()
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
        unreachable!()
    }
    async fn create_change(&self, _: &Path, _: &str) -> Result<CubeChangeHandle> {
        unreachable!()
    }
    async fn goto_workspace(&self, _: &Path, _: u64) -> Result<()> {
        unreachable!()
    }
    async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
        unreachable!()
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        unreachable!()
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        unreachable!()
    }
    async fn spawn_worker(
        &self,
        _: &str,
        _: &WorkExecution,
        _: &WorkItem,
        _: &Path,
        _: Option<&str>,
    ) -> Result<RunOutcome> {
        unreachable!()
    }
    async fn deliver_worker_input(&self, _: &str, kind: WorkerInputKind, _: &str) -> Result<ControlOutcome> {
        self.calls.lock().unwrap().push(kind);
        Ok(self.outcome.clone())
    }
}

fn spoke_state(remote: Arc<Remote>) -> (Arc<super::ServerState>, tempfile::TempDir) {
    let (state, dir) = test_server_state_with_overrides(ServerStateOverrides {
        host_adapter_provider: Some(Arc::new(Provider(remote))),
        ..Default::default()
    });
    state.work_db.add_host("spoke", "worker@spoke", 1, &[]).unwrap();
    state
        .work_db
        .set_host_transport("spoke", boss_work_db::HostTransport::SpokeReady)
        .unwrap();
    (state, dir)
}

fn delivered() -> ControlOutcome {
    ControlOutcome::Delivered(WorkerPane {
        life: WorkerLife::Alive,
        session: "worker".into(),
        pane_id: "%1".into(),
        pid: Some(42),
        exit_code: None,
    })
}

#[tokio::test]
async fn queued_probe_uses_host_on_stop_without_a_local_slot_and_never_duplicates_uncertain_input() {
    for (outcome, expected) in [
        (
            ControlOutcome::Uncertain("reply lost".into()),
            ProbeDeliveryState::Unconfirmed,
        ),
        (delivered(), ProbeDeliveryState::Buffered),
    ] {
        let remote = Arc::new(Remote {
            outcome,
            calls: Mutex::new(vec![]),
        });
        let (state, _dir) = spoke_state(remote.clone());
        state.work_db.reserve_spoke_assignment("execution", "spoke").unwrap();
        let probe = state.queue_probe("execution".into(), "continue".into(), false);
        assert!(state.worker_registry.slot_for_run("execution").is_none());
        let stop = boss_engine_runtime::events_socket::IncomingHookEvent::for_test(
            boss_protocol::WorkerEvent::Stop {
                session_id: "remote-session".into(),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            },
            Some("execution".into()),
            None,
        );
        super::worker_events::dispatch_probe_on_stop(&state, &stop).await;
        assert_eq!(state.probe_lifecycle_state(&probe), Some(expected));
        dispatch_probe_now(&state, "execution").await;
        assert_eq!(remote.calls.lock().unwrap().as_slice(), &[WorkerInputKind::Probe]);
        assert!(state.has_in_flight_probe("execution"));
    }
}

#[tokio::test]
async fn unsent_spoke_probe_stays_queued_without_a_local_slot() {
    let remote = Arc::new(Remote {
        outcome: ControlOutcome::NotSubmitted("offline".into()),
        calls: Mutex::new(vec![]),
    });
    let (state, _dir) = spoke_state(remote.clone());
    state.work_db.reserve_spoke_assignment("execution", "spoke").unwrap();
    let probe = state.queue_probe("execution".into(), "continue".into(), false);
    let outcome = dispatch_probe_now(&state, "execution").await;
    assert_eq!(outcome, ProbeDispatchOutcome::PostureRefused);
    assert_eq!(state.probe_lifecycle_state(&probe), Some(ProbeDeliveryState::Queued));
    assert!(state.has_pending_probe("execution"));
    assert!(!state.has_in_flight_probe("execution"));
    assert!(remote.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn post_tool_use_does_not_type_a_spoke_probe_into_a_busy_pane() {
    let remote = Arc::new(Remote {
        outcome: delivered(),
        calls: Mutex::new(vec![]),
    });
    let (state, _dir) = spoke_state(remote.clone());
    let run_id = register_working_worker_with_driver(&state, 5, Some("grok"));
    state.work_db.reserve_spoke_assignment(&run_id, "spoke").unwrap();
    let probe = state.queue_probe(run_id.clone(), "continue".into(), false);
    let post_tool_use = boss_engine_runtime::events_socket::IncomingHookEvent::for_test(
        boss_protocol::WorkerEvent::PostToolUse {
            session_id: "remote-session".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
        Some(run_id.clone()),
        None,
    );
    let outcome = dispatch_probe_on_post_tool_use(&state, &post_tool_use).await;
    assert_eq!(outcome, ProbeDispatchOutcome::PostureRefused);
    assert_eq!(
        dispatch_probe_now(&state, &run_id).await,
        ProbeDispatchOutcome::PostureRefused
    );
    let nudge = state
        .send_input_to_worker(&run_id, "nudge".into())
        .await
        .expect_err("mid-turn spoke nudge must be refused");
    assert!(matches!(nudge, SendInputError::NotAcceptingInput { .. }), "{nudge:?}");
    assert_eq!(state.probe_lifecycle_state(&probe), Some(ProbeDeliveryState::Queued));
    assert!(remote.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn probe_now_delivers_to_a_parked_spoke_worker() {
    let remote = Arc::new(Remote {
        outcome: delivered(),
        calls: Mutex::new(vec![]),
    });
    let (state, _dir) = spoke_state(remote.clone());
    let run_id = register_idle_worker_with_driver(&state, 6, Some("grok"));
    state.work_db.reserve_spoke_assignment(&run_id, "spoke").unwrap();
    let probe = state.queue_probe(run_id.clone(), "continue".into(), false);
    let outcome = dispatch_probe_now(&state, &run_id).await;
    assert_eq!(outcome, ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Buffered));
    assert_eq!(state.probe_lifecycle_state(&probe), Some(ProbeDeliveryState::Buffered));
    assert_eq!(remote.calls.lock().unwrap().as_slice(), &[WorkerInputKind::Probe]);
}

#[tokio::test]
async fn releasing_a_terminal_spoke_run_settles_queued_and_in_flight_probes() {
    use boss_engine_runtime::test_support::{create_active_chore, create_old_execution, create_product};

    let remote = Arc::new(Remote {
        outcome: delivered(),
        calls: Mutex::new(vec![]),
    });
    let (state, _dir) = spoke_state(remote);
    let product_id = create_product(&state.work_db);
    let work_item_id = create_active_chore(&state.work_db, &product_id, "spoke probe settle");
    let run_id = create_old_execution(&state.work_db, &work_item_id);
    state.work_db.reserve_spoke_assignment(&run_id, "spoke").unwrap();
    state
        .work_db
        .start_execution_run_on_host(&run_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws", "spoke")
        .unwrap();
    state
        .work_db
        .mark_execution_orphaned(&run_id, "terminal for probe settlement")
        .unwrap();
    let in_flight = state.queue_probe(run_id.clone(), "in flight".into(), false);
    let queued = state.queue_probe(run_id.clone(), "queued".into(), false);
    let claimed = state
        .try_reserve_probe_for_delivery(&run_id, None, 0)
        .expect("in-flight reservation");
    assert_eq!(claimed.probe_id, in_flight);
    state.set_probe_lifecycle(&in_flight, ProbeDeliveryState::Buffered);
    assert_eq!(
        state.release_proven_dead_remote_pane_for_test(&run_id).await,
        PaneReleaseOutcome::Reaped
    );
    assert_eq!(
        state.probe_lifecycle_state(&queued),
        Some(ProbeDeliveryState::Abandoned)
    );
    assert_eq!(
        state.probe_lifecycle_state(&in_flight),
        Some(ProbeDeliveryState::Orphaned)
    );
}
