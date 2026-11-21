use super::*;
use crate::test_support::*;
use boss_execution_contracts::WorkerPool;
use boss_protocol::WorkerEvent;
use std::sync::Mutex;

pub(super) const SLOT: u8 = 41;

#[derive(Clone)]
pub(super) struct RecordingReaper {
    coordinator: Arc<ExecutionCoordinator>,
    calls: Arc<Mutex<Vec<(String, bool)>>>,
    db: Arc<WorkDb>,
}

impl RecordingReaper {
    pub(super) fn new(coordinator: Arc<ExecutionCoordinator>, db: Arc<WorkDb>) -> Self {
        Self {
            coordinator,
            db,
            calls: Arc::default(),
        }
    }
    pub(super) fn reaped(&self) -> Vec<(String, bool)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl WorkerPaneReleaser for RecordingReaper {
    async fn release_pane(&self, execution_id: &str) -> PaneReleaseOutcome {
        let claimed = self
            .coordinator
            .progression_worker_pool()
            .claimed_execution_ids()
            .await
            .contains(execution_id);
        self.calls.lock().unwrap().push((execution_id.to_owned(), claimed));
        let db = &self.db;
        if let Some(identity) = db.tmux_identity_for_execution(execution_id).unwrap() {
            db.clear_tmux_identity_for_execution(execution_id, &identity.spawn_token)
                .unwrap();
            PaneReleaseOutcome::Reaped
        } else {
            PaneReleaseOutcome::NoLiveWorker
        }
    }
}

pub(super) struct NoViewers;
#[async_trait::async_trait]
impl WorkerViewerDetach for NoViewers {
    async fn confirm_process_torn_down(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn confirm_viewers_detached(&self, ids: &[String]) -> Vec<Result<(), String>> {
        ids.iter().map(|_| Ok(())).collect()
    }
}

pub(super) fn description_of(db: &WorkDb, id: &str) -> String {
    match db.get_work_item(id).unwrap() {
        boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => t.description,
        _ => panic!("expected a chore"),
    }
}

/// A live progression run started five minutes ago on an active chore, with
/// the engine's published timeout set to `timeout_secs`.
pub(super) async fn progression_slot(
    timeout_secs: Option<i64>,
    drive_working: bool,
) -> (
    tempfile::TempDir,
    Arc<WorkDb>,
    String,
    String,
    Arc<LiveWorkerStateRegistry>,
    Arc<ExecutionCoordinator>,
) {
    let (dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "stuck chore");
    let db = Arc::new(db);
    let execution_id = create_old_execution(&db, &work_item_id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'progression' WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    let (_, run) = db
        .start_progression_run_on_local_host(&execution_id, "progression-1", "/tmp/progression-timeout-fixture", true)
        .unwrap();
    finish_run_worker_pane_alive(&db, &execution_id, &run.id, Some("spawned pane"));
    db.record_tmux_spawn_intent_for_execution(&execution_id, "boss", "boss-progression-1", "deadline-token")
        .unwrap();
    if let Some(secs) = timeout_secs {
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO metadata (key, value) VALUES ('progression.run_timeout_secs', ?1)",
                [secs.to_string()],
            )
            .unwrap();
    }
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    live_states.register_spawn(SLOT, &execution_id, "claude-sonnet", 1, None);
    if drive_working {
        live_states.apply_event(
            SLOT,
            &WorkerEvent::PreToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
            },
        );
    }
    let mut coordinator = make_coordinator(db.clone(), 1);
    Arc::get_mut(&mut coordinator)
        .unwrap()
        .set_progression_pool(WorkerPool::new_progression(1));
    coordinator
        .progression_worker_pool()
        .claim_progression_worker(&execution_id, 1)
        .await
        .unwrap();
    (dir, db, work_item_id, execution_id, live_states, coordinator)
}

pub(super) async fn progression_fixture(
    timeout_secs: Option<i64>,
) -> (
    tempfile::TempDir,
    Arc<WorkDb>,
    String,
    String,
    Arc<LiveWorkerStateRegistry>,
    Arc<ExecutionCoordinator>,
) {
    progression_slot(timeout_secs, true).await
}

pub(super) async fn sweep(
    db: &Arc<WorkDb>,
    live_states: &Arc<LiveWorkerStateRegistry>,
    coordinator: &Arc<ExecutionCoordinator>,
    reaper: &RecordingReaper,
) -> usize {
    ProgressionTimeoutSweep::builder()
        .work_db(db.clone())
        .live_states(live_states.clone())
        .coordinator(coordinator.clone())
        .pane_releaser(Arc::new(reaper.clone()))
        .hold_registry(Arc::new(crate::hold_registry::HoldRegistry::new()))
        .dispatch_events(Arc::new(crate::dispatch_events::NoopDispatchEventSink))
        .viewers(Arc::new(NoViewers))
        .build()
        .run_one_pass(boss_engine_utils::epoch_time::now_epoch_secs())
        .await
}

pub(super) fn age_terminal(db: &WorkDb, execution: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET finished_at = '1000' WHERE id = ?1",
            [execution],
        )
        .unwrap();
}

pub(super) struct UnconfirmedProcess;

#[async_trait::async_trait]
impl WorkerPaneReleaser for UnconfirmedProcess {
    async fn release_pane(&self, _: &str) -> PaneReleaseOutcome {
        PaneReleaseOutcome::NoLiveWorker
    }
}

pub(super) struct UnconfirmedViewer;
#[async_trait::async_trait]
impl WorkerViewerDetach for UnconfirmedViewer {
    async fn confirm_process_torn_down(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn confirm_viewers_detached(&self, ids: &[String]) -> Vec<Result<(), String>> {
        ids.iter().map(|_| Err("viewer unavailable".into())).collect()
    }
}
