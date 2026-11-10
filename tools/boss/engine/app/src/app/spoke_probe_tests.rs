use super::{ServerStateOverrides, tests::test_server_state_with_overrides, worker_events::dispatch_probe_now};
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

#[tokio::test]
async fn queued_probe_uses_host_without_local_slot_and_never_duplicates_uncertain_input() {
    for (outcome, expected) in [
        (
            ControlOutcome::Uncertain("reply lost".into()),
            ProbeDeliveryState::Unconfirmed,
        ),
        (
            ControlOutcome::Delivered(WorkerPane {
                life: WorkerLife::Alive,
                session: "worker".into(),
                pane_id: "%1".into(),
                pid: Some(42),
                exit_code: None,
            }),
            ProbeDeliveryState::Buffered,
        ),
    ] {
        let remote = Arc::new(Remote {
            outcome,
            calls: Mutex::new(vec![]),
        });
        let (state, _dir) = test_server_state_with_overrides(ServerStateOverrides {
            host_adapter_provider: Some(Arc::new(Provider(remote.clone()))),
            ..Default::default()
        });
        state.work_db.add_host("spoke", "worker@spoke", 1, &[]).unwrap();
        state
            .work_db
            .set_host_transport("spoke", boss_work_db::HostTransport::SpokeReady)
            .unwrap();
        state.work_db.reserve_spoke_assignment("execution", "spoke").unwrap();
        let probe = state.queue_probe("execution".into(), "continue".into(), false);
        dispatch_probe_now(&state, "execution").await;
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
    let (state, _dir) = test_server_state_with_overrides(ServerStateOverrides {
        host_adapter_provider: Some(Arc::new(Provider(remote))),
        ..Default::default()
    });
    state.work_db.add_host("spoke", "worker@spoke", 1, &[]).unwrap();
    state
        .work_db
        .set_host_transport("spoke", boss_work_db::HostTransport::SpokeReady)
        .unwrap();
    state.work_db.reserve_spoke_assignment("execution", "spoke").unwrap();
    let probe = state.queue_probe("execution".into(), "continue".into(), false);
    dispatch_probe_now(&state, "execution").await;
    assert_eq!(state.probe_lifecycle_state(&probe), Some(ProbeDeliveryState::Queued));
    assert!(state.has_pending_probe("execution"));
    assert!(!state.has_in_flight_probe("execution"));
}
