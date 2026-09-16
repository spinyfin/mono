//! Discovery lifecycle: what the ingress records while it has no rollout.
//!
//! Pins the behaviour the 2026-09-13 breaker incident turned on — a rollout
//! that appears *after* the overdue threshold still gets attached, and every
//! unattached state leaves a durable, attributable record — see
//! [`super::DISCOVERY_OVERDUE_AFTER`].

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::Notify;

use super::{
    AgentJsonlProgressManager, DiscoveryRecord, DiscoveryVerdict, IngressCheckpoint, IngressCheckpointStore,
    IngressObservation,
};
use crate::driver::AgentJsonlFileIngress;
use crate::events_socket::IncomingHookEvent;
use crate::stdout_progress::WorkerEventSink;

#[tokio::test]
async fn rejected_candidate_after_overdue_refreshes_diagnostics_and_can_recover() {
    let fx = armed_run("run-diagnostic", Duration::from_millis(100));
    fx.sink
        .wait_for(|obs| {
            obs.iter()
                .any(|o| matches!(o, IngressObservation::DiscoveryOverdue { .. }))
        })
        .await;
    let path = fx.sessions().join("rollout-diagnostic-thread.jsonl");
    fs::write(
        &path,
        vec![b'x'; crate::agent_jsonl_discovery::MAX_SESSION_META_BYTES as usize + 1],
    )
    .unwrap();
    fx.sink
        .wait_for(|_| {
            discovery_record(fx.store.get("run-diagnostic")).is_some_and(|r| {
                r.rejections
                    .first()
                    .is_some_and(|r| r.reason == super::CandidateRejectReason::OversizedSessionMeta)
            })
        })
        .await;
    let oversized = discovery_record(fx.store.get("run-diagnostic")).unwrap();
    assert_eq!(oversized.rejected_candidates, 1);
    assert_eq!(oversized.rejections[0].file_name, "rollout-diagnostic-thread.jsonl");
    assert!(oversized.waited_secs >= 1);
    fs::write(&path, b"{").unwrap();
    fx.sink
        .wait_for(|_| {
            discovery_record(fx.store.get("run-diagnostic")).is_some_and(|r| {
                r.rejections
                    .first()
                    .is_some_and(|r| r.reason == super::CandidateRejectReason::IncompleteSessionMeta)
            })
        })
        .await;
    fs::remove_file(&path).unwrap();
    // Removing the file does not erase its history: `rejected_candidates`
    // and `rejections` are cumulative across the whole discovery window
    // (matching `reason`), so a file that rotated away mid-window is still
    // named rather than silently dropping the count back to zero.
    let removed = discovery_record(fx.store.get("run-diagnostic")).unwrap();
    assert_eq!(removed.rejected_candidates, 1);
    assert_eq!(removed.rejections[0].file_name, "rollout-diagnostic-thread.jsonl");
    assert!(
        removed.reason.contains("rollout-diagnostic-thread.jsonl"),
        "{}",
        removed.reason
    );
    assert!(removed.reason.contains("were seen"), "{}", removed.reason);
    fs::write(&path, rollout(&fx.workspace(), "thread")).unwrap();
    fx.sink
        .wait_for(|obs| obs.iter().any(|o| matches!(o, IngressObservation::Attached { .. })))
        .await;
    fx.manager.stop_run("run-diagnostic");
}

/// Captures both the fan-out events and the ingress observations.
#[derive(Clone, Default)]
struct ObservingSink {
    events: Arc<Mutex<Vec<IncomingHookEvent>>>,
    observations: Arc<Mutex<Vec<IngressObservation>>>,
    attached_runs: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait::async_trait]
impl WorkerEventSink for ObservingSink {
    fn record_driver_attach(&self, run_id: &str) {
        self.attached_runs.lock().unwrap().push(run_id.to_owned());
        self.notify.notify_waiters();
    }

    async fn dispatch_worker_event(&self, incoming: IncomingHookEvent) {
        self.events.lock().unwrap().push(incoming);
        self.notify.notify_waiters();
    }

    async fn record_ingress_observation(&self, _run_id: &str, observation: IngressObservation) {
        self.observations.lock().unwrap().push(observation);
        self.notify.notify_waiters();
    }
}

impl ObservingSink {
    fn observations(&self) -> Vec<IngressObservation> {
        self.observations.lock().unwrap().clone()
    }

    /// Wait until `pred` holds over the observations recorded so far.
    async fn wait_for(&self, pred: impl Fn(&[IngressObservation]) -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if pred(&self.observations()) {
                    return;
                }
                let _ = tokio::time::timeout(Duration::from_millis(50), self.notify.notified()).await;
            }
        })
        .await
        .expect("observation should arrive");
    }
}

#[derive(Clone, Default)]
struct MemoryStore {
    stored: Arc<Mutex<HashMap<String, IngressCheckpoint>>>,
}

impl MemoryStore {
    fn get(&self, run_id: &str) -> Option<IngressCheckpoint> {
        self.stored.lock().unwrap().get(run_id).cloned()
    }
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
        Ok(self.get(run_id))
    }
}

struct Fixture {
    temp: TempDir,
    manager: AgentJsonlProgressManager,
    sink: ObservingSink,
    store: MemoryStore,
}

impl Fixture {
    /// The per-run sessions directory the ingress watches.
    fn sessions(&self) -> PathBuf {
        self.temp.path().join("sessions")
    }

    /// The workspace a rollout's `session_meta.cwd` must resolve to.
    fn workspace(&self) -> PathBuf {
        self.temp.path().join("workspace")
    }
}

/// A prepared and activated run whose overdue threshold is milliseconds.
fn armed_run(run_id: &str, overdue_after: Duration) -> Fixture {
    let temp = TempDir::new().unwrap();
    let sessions = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let manager = AgentJsonlProgressManager::new().with_discovery_overdue_after(overdue_after);
    let sink = ObservingSink::default();
    let store = MemoryStore::default();
    manager
        .prepare_run(
            run_id,
            Arc::new(crate::driver::CodexDriver::default()),
            AgentJsonlFileIngress {
                directory: sessions.clone(),
                filename_prefix: "rollout-".into(),
                filename_suffix: ".jsonl".into(),
                workspace_path: workspace.clone(),
            },
            sink.clone(),
            Arc::new(store.clone()),
        )
        .unwrap();
    manager.activate_run(run_id);
    Fixture {
        temp,
        manager,
        sink,
        store,
    }
}

fn rollout(workspace: &Path, thread_id: &str) -> String {
    let meta = serde_json::json!({
        "type": "session_meta",
        "payload": { "id": thread_id, "cwd": workspace }
    });
    format!("{}\n", serde_json::to_string(&meta).unwrap())
}

fn discovery_record(checkpoint: Option<IngressCheckpoint>) -> Option<DiscoveryRecord> {
    match checkpoint {
        Some(IngressCheckpoint::Armed { discovery, .. }) => discovery,
        other => panic!("expected an Armed checkpoint, got {other:?}"),
    }
}

/// The incident shape: the rollout shows up after the threshold. Discovery
/// must report itself overdue — durably and to the sink — and then attach
/// anyway rather than having already given up.
#[tokio::test]
async fn rollout_appearing_after_the_overdue_threshold_still_attaches() {
    let fx = armed_run("run-late", Duration::from_millis(200));

    fx.sink
        .wait_for(|obs| {
            obs.iter()
                .any(|o| matches!(o, IngressObservation::DiscoveryOverdue { .. }))
        })
        .await;
    let record = discovery_record(fx.store.get("run-late")).expect("overdue verdict is recorded on the checkpoint");
    assert_eq!(record.verdict, DiscoveryVerdict::Overdue);
    assert_eq!(record.rejected_candidates, 0);
    assert!(record.reason.contains("still looking"), "{}", record.reason);
    assert!(
        fx.sink.events.lock().unwrap().is_empty(),
        "nothing to dispatch before a rollout exists"
    );

    // Now the driver finally writes its rollout — the equivalent of the
    // incident's 123–126s startups against the old 120s give-up point.
    let path = fx.sessions().join("rollout-late-thread-late.jsonl");
    fs::write(&path, rollout(&fx.workspace(), "thread-late")).unwrap();

    fx.sink
        .wait_for(|obs| obs.iter().any(|o| matches!(o, IngressObservation::Attached { .. })))
        .await;
    let attached = fx
        .sink
        .observations()
        .into_iter()
        .find_map(|o| match o {
            IngressObservation::Attached {
                path,
                session_id,
                discovery_secs,
            } => Some((path, session_id, discovery_secs)),
            _ => None,
        })
        .unwrap();
    assert_eq!(attached.0, fs::canonicalize(&path).unwrap());
    assert_eq!(attached.1, "thread-late");
    assert!(
        attached.2.is_some(),
        "a discovered attach reports how long discovery took"
    );
    assert!(
        matches!(fx.store.get("run-late"), Some(IngressCheckpoint::Attached { session_id, .. }) if session_id == "thread-late"),
        "the checkpoint is promoted to Attached once the rollout is found"
    );

    // Only one overdue notice, however long it took.
    let overdue_count = fx
        .sink
        .observations()
        .iter()
        .filter(|o| matches!(o, IngressObservation::DiscoveryOverdue { .. }))
        .count();
    assert_eq!(overdue_count, 1);
    fx.manager.stop_run("run-late");
}

/// A rollout that exists but is not this run's is a different failure from
/// "no rollout yet", and the overdue record says which.
#[tokio::test]
async fn overdue_record_counts_rollouts_that_failed_correlation() {
    let fx = armed_run("run-wrong-cwd", Duration::from_millis(200));
    let elsewhere = fx.temp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(
        fx.sessions().join("rollout-x-thread-x.jsonl"),
        rollout(&elsewhere, "thread-x"),
    )
    .unwrap();

    fx.sink
        .wait_for(|obs| {
            obs.iter().any(|o| {
                matches!(
                    o,
                    IngressObservation::DiscoveryOverdue {
                        rejected_candidates: 1,
                        ..
                    }
                )
            })
        })
        .await;
    let record = discovery_record(fx.store.get("run-wrong-cwd")).unwrap();
    assert_eq!(record.verdict, DiscoveryVerdict::Overdue);
    assert_eq!(record.rejected_candidates, 1);
    fx.manager.stop_run("run-wrong-cwd");
}

/// Two new rollouts both correlating to one run is a failure polling cannot
/// cure. It is still recorded rather than merely logged.
#[tokio::test]
async fn ambiguous_rollouts_record_a_failed_discovery() {
    let fx = armed_run("run-ambiguous", Duration::from_secs(60));
    fs::write(
        fx.sessions().join("rollout-a-thread-a.jsonl"),
        rollout(&fx.workspace(), "thread-a"),
    )
    .unwrap();
    fs::write(
        fx.sessions().join("rollout-b-thread-b.jsonl"),
        rollout(&fx.workspace(), "thread-b"),
    )
    .unwrap();

    fx.sink
        .wait_for(|obs| {
            obs.iter()
                .any(|o| matches!(o, IngressObservation::DiscoveryFailed { .. }))
        })
        .await;
    let record = discovery_record(fx.store.get("run-ambiguous")).unwrap();
    assert_eq!(record.verdict, DiscoveryVerdict::Failed);
    assert!(record.reason.contains("ambiguous"), "{}", record.reason);
    assert!(fx.sink.events.lock().unwrap().is_empty());
    fx.manager.stop_run("run-ambiguous");
}

/// Rows written before the record existed must still load, and a record
/// must survive the round trip through the `work_runs` column.
#[test]
fn armed_checkpoint_serialization_is_backward_compatible() {
    let legacy = serde_json::json!({
        "kind": "armed",
        "ingress": {
            "directory": "/tmp/sessions",
            "filename_prefix": "rollout-",
            "filename_suffix": ".jsonl",
            "workspace_path": "/tmp/workspace"
        },
        "baseline": []
    });
    let parsed: IngressCheckpoint = serde_json::from_value(legacy).unwrap();
    assert!(matches!(parsed, IngressCheckpoint::Armed { discovery: None, .. }));

    let with_record = IngressCheckpoint::Armed {
        ingress: AgentJsonlFileIngress {
            directory: "/tmp/sessions".into(),
            filename_prefix: "rollout-".into(),
            filename_suffix: ".jsonl".into(),
            workspace_path: "/tmp/workspace".into(),
        },
        baseline: Vec::new(),
        discovery: Some(DiscoveryRecord {
            verdict: DiscoveryVerdict::Overdue,
            at_epoch_secs: 1_789_335_748,
            waited_secs: 120,
            rejected_candidates: 0,
            rejections: Vec::new(),
            reason: "still looking".into(),
        }),
    };
    let json = serde_json::to_string(&with_record).unwrap();
    assert!(json.contains("\"verdict\":\"overdue\""), "{json}");
    let back: IngressCheckpoint = serde_json::from_str(&json).unwrap();
    let record = discovery_record(Some(back)).unwrap();
    assert_eq!(record.verdict, DiscoveryVerdict::Overdue);
    assert_eq!(record.waited_secs, 120);
}
