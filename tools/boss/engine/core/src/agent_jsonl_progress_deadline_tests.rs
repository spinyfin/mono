use super::*;
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use crate::driver::AgentJsonlFileIngress;
use crate::events_socket::IncomingHookEvent;
use crate::stdout_progress::WorkerEventSink;

#[derive(Clone, Default)]
struct ObservingSink {
    observations: Arc<Mutex<Vec<IngressObservation>>>,
    notify: Arc<Notify>,
}

#[async_trait::async_trait]
impl WorkerEventSink for ObservingSink {
    fn record_driver_attach(&self, _run_id: &str) {}

    async fn dispatch_worker_event(&self, _incoming: IncomingHookEvent) {}

    async fn record_ingress_observation(&self, _run_id: &str, observation: IngressObservation) {
        self.observations.lock().unwrap().push(observation);
        self.notify.notify_waiters();
    }
}

impl ObservingSink {
    fn observations(&self) -> Vec<IngressObservation> {
        self.observations.lock().unwrap().clone()
    }
}

#[derive(Clone, Default)]
struct MemoryStore {
    stored: Arc<Mutex<HashMap<String, IngressCheckpoint>>>,
}

impl IngressCheckpointStore for MemoryStore {
    fn store_ingress_checkpoint(&self, run_id: &str, checkpoint: &IngressCheckpoint) -> Result<(), String> {
        self.stored
            .lock()
            .unwrap()
            .insert(run_id.to_owned(), checkpoint.clone());
        Ok(())
    }

    fn load_ingress_checkpoint(&self, run_id: &str) -> Result<Option<IngressCheckpoint>, String> {
        Ok(self.stored.lock().unwrap().get(run_id).cloned())
    }
}

fn arm_run(temp: &tempfile::TempDir, manager: &AgentJsonlProgressManager, run_id: &str, sink: ObservingSink, store: MemoryStore) {
    let directory = temp.path().join("sessions");
    let workspace_path = temp.path().join("workspace");
    fs::create_dir_all(&directory).unwrap();
    fs::create_dir_all(&workspace_path).unwrap();
    manager
        .prepare_run(
            run_id,
            Arc::new(crate::driver::CodexDriver::default()),
            AgentJsonlFileIngress {
                directory,
                filename_prefix: "rollout-".into(),
                filename_suffix: ".jsonl".into(),
                workspace_path,
            },
            sink,
            Arc::new(store),
        )
        .unwrap();
    manager.activate_run(run_id);
}

fn rollout(workspace: &std::path::Path, thread_id: &str) -> String {
    let meta = serde_json::json!({
        "type": "session_meta",
        "payload": { "id": thread_id, "cwd": workspace }
    });
    format!("{}\n", serde_json::to_string(&meta).unwrap())
}

#[tokio::test(start_paused = true)]
async fn first_discovery_observes_later_peers_and_attaches_past_old_deadline() {
    let temp = tempfile::TempDir::new().unwrap();
    let manager = AgentJsonlProgressManager::new();
    let sink = ObservingSink::default();
    let store = MemoryStore::default();
    arm_run(&temp, &manager, "late", sink.clone(), store.clone());
    tokio::task::yield_now().await;
    let peers: Vec<_> = (0..5).map(|_| manager.discovery_load().begin()).collect();
    tokio::time::advance(Duration::from_secs(122)).await;
    tokio::task::yield_now().await;
    assert!(
        !sink
            .observations()
            .iter()
            .any(|o| matches!(o, IngressObservation::DiscoveryOverdue { .. })),
        "burst discovery must survive 120s without reporting overdue"
    );
    drop(peers);
    let path = temp.path().join("sessions").join("rollout-late.jsonl");
    fs::write(path, rollout(&temp.path().join("workspace"), "late")).unwrap();
    tokio::time::advance(Duration::from_millis(200)).await;
    tokio::task::yield_now().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if sink
                .observations()
                .iter()
                .any(|o| matches!(o, IngressObservation::Attached { .. }))
            {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), sink.notify.notified()).await;
        }
    })
    .await
    .expect("late rollout should attach after the old 120s deadline");
    manager.stop_run("late");
}

#[tokio::test(start_paused = true)]
async fn dead_discovery_reports_overdue_at_the_bounded_deadline() {
    let temp = tempfile::TempDir::new().unwrap();
    let manager = AgentJsonlProgressManager::new();
    let sink = ObservingSink::default();
    let store = MemoryStore::default();
    arm_run(&temp, &manager, "dead", sink.clone(), store.clone());
    tokio::task::yield_now().await;
    let _peers: Vec<_> = (0..100).map(|_| manager.discovery_load().begin()).collect();
    tokio::time::advance(Duration::from_secs(239)).await;
    tokio::task::yield_now().await;
    assert!(
        !sink
            .observations()
            .iter()
            .any(|o| matches!(o, IngressObservation::DiscoveryOverdue { .. }))
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if sink
                .observations()
                .iter()
                .any(|o| matches!(o, IngressObservation::DiscoveryOverdue { .. }))
            {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), sink.notify.notified()).await;
        }
    })
    .await
    .expect("capped contention must report overdue at 240s");
    assert!(
        !sink
            .observations()
            .iter()
            .any(|o| matches!(o, IngressObservation::DiscoveryFailed { .. })),
        "discovery keeps polling after the overdue report"
    );
    manager.stop_run("dead");
}
