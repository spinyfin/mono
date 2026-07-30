use super::server::current_parent_pid;
use super::*;
use crate::protocol::TopicEventPayload;

fn topic_envelope(topic: &str, revision: u64) -> FrontendEventEnvelope {
    FrontendEventEnvelope::push_with_revision(
        revision,
        FrontendEvent::TopicEvent {
            topic: topic.to_owned(),
            revision,
            origin_session_id: "test".to_owned(),
            origin_request_id: None,
            event: TopicEventPayload::WorkInvalidated {
                reason: "test".to_owned(),
                product_id: None,
                item_ids: vec![],
            },
        },
    )
}

fn response_envelope(request_id: &str) -> FrontendEventEnvelope {
    FrontendEventEnvelope::response(request_id.to_owned(), FrontendEvent::ProductsList { products: vec![] })
}

/// A priority-lane envelope: the small engine→app control push
/// (`EngineRequest`) that `send_to_app` issues and blocks on. Used to
/// exercise the priority lane independently of the bulk lane.
fn engine_request_envelope(request_id: &str) -> FrontendEventEnvelope {
    FrontendEventEnvelope::push(FrontendEvent::EngineRequest {
        request_id: request_id.to_owned(),
        request: EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput {
            slot_id: 1,
            kill_grace_seconds: 5,
        }),
    })
}

fn topic_of(env: &FrontendEventEnvelope) -> Option<String> {
    topic_event_topic(&env.payload)
}

/// Build a `ServerState` backed by a throwaway on-disk DB. The returned
/// `TempDir` must be kept alive for as long as the `ServerState` is used —
/// dropping it deletes the backing `state.db`.
pub(super) fn test_server_state() -> (Arc<ServerState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build(),
        None,
    ));
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, None, None, None, None).unwrap();
    (state, temp)
}

pub(super) fn make_session_sink() -> Arc<SessionSink> {
    let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
    Arc::new(SessionSink::new(shutdown_tx))
}

/// Register `run_id` → `slot_id` and seed live state at
/// [`boss_protocol::WorkerActivity::Idle`] so
/// [`ServerState::inject_pane_text_verified`] / `send_input_to_worker`
/// accept typed input. Production spawn leaves activity at `Spawning`
/// until the first SessionStart/Stop; tests that only exercise the
/// pane-write path skip that lifecycle and park the worker at Idle.
pub(super) fn register_idle_worker(server_state: &ServerState, run_id: &str, slot_id: u8) {
    use boss_protocol::{WorkerActivity, WorkerEvent};
    server_state.worker_registry.register_run_slot(run_id, slot_id);
    server_state
        .live_worker_states
        .register_spawn(slot_id, run_id.to_owned(), "claude-opus-4-7", 0, None);
    server_state.live_worker_states.apply_event(
        slot_id,
        &WorkerEvent::Stop {
            session_id: "test-sess".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    assert_eq!(
        server_state.live_worker_states.get(slot_id).unwrap().activity,
        WorkerActivity::Idle,
        "register_idle_worker precondition: slot must be Idle",
    );
}

/// Like [`register_idle_worker`], but leaves activity at
/// [`boss_protocol::WorkerActivity::Working`] (a PreToolUse with no
/// balancing Stop). Used to assert the typed-input guard refuses
/// mid-turn injection.
pub(super) fn register_working_worker(server_state: &ServerState, run_id: &str, slot_id: u8) {
    use boss_protocol::{WorkerActivity, WorkerEvent};
    server_state.worker_registry.register_run_slot(run_id, slot_id);
    server_state
        .live_worker_states
        .register_spawn(slot_id, run_id.to_owned(), "claude-opus-4-7", 0, None);
    server_state.live_worker_states.apply_event(
        slot_id,
        &WorkerEvent::PreToolUse {
            session_id: "test-sess".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
        },
    );
    assert_eq!(
        server_state.live_worker_states.get(slot_id).unwrap().activity,
        WorkerActivity::Working,
        "register_working_worker precondition: slot must be Working",
    );
}

/// Seed a product → chore → execution chain so
/// `WorkDb::get_execution_driver_slug` resolves for the returned execution id,
/// and register that execution against `slot_id` as a **mid-turn** worker.
///
/// `driver` sets `tasks.driver`; `None` leaves it unset so the engine default
/// (`claude`) applies. This is the fixture the mid-turn pane-input decision
/// needs, because that decision reads the run's driver — a bare `run_id` with
/// no execution row resolves to no driver and fails closed, which is correct
/// behaviour but tests nothing about either driver.
pub(super) fn register_working_worker_with_driver(
    server_state: &ServerState,
    slot_id: u8,
    driver: Option<&str>,
) -> String {
    let execution_id = execution_id_with_driver(server_state, driver);
    register_working_worker(server_state, &execution_id, slot_id);
    execution_id
}

/// Like [`register_working_worker_with_driver`], but leaves the slot at
/// [`boss_protocol::WorkerActivity::Spawning`] — a worker whose pane exists
/// but which has not reached a tool boundary yet. Used to pin which delivery
/// boundary the engine promises before a worker's first turn starts.
pub(super) fn register_spawning_worker_with_driver(
    server_state: &ServerState,
    slot_id: u8,
    driver: Option<&str>,
) -> String {
    use boss_protocol::WorkerActivity;

    let execution_id = execution_id_with_driver(server_state, driver);
    server_state.worker_registry.register_run_slot(&execution_id, slot_id);
    server_state
        .live_worker_states
        .register_spawn(slot_id, execution_id.clone(), "claude-opus-4-7", 0, None);
    assert_eq!(
        server_state.live_worker_states.get(slot_id).unwrap().activity,
        WorkerActivity::Spawning,
        "register_spawning_worker_with_driver precondition: slot must be Spawning",
    );
    execution_id
}

/// Seed a product → chore → execution chain whose `tasks.driver` is `driver`
/// (`None` leaves it unset so the engine default, `claude`, applies), and
/// return the execution id.
fn execution_id_with_driver(server_state: &ServerState, driver: Option<&str>) -> String {
    use boss_protocol::RequestExecutionInput;

    let product = crate::test_support::create_test_product_with_repo(
        &server_state.work_db,
        "probe-posture-product",
        Some("git@example.com:p.git"),
    );
    let chore = crate::test_support::create_test_chore_manual(&server_state.work_db, product.id.clone(), "c");
    if let Some(driver) = driver {
        server_state
            .work_db
            .connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET driver = ?2 WHERE id = ?1",
                rusqlite::params![chore.id, driver],
            )
            .unwrap();
    }
    server_state
        .work_db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap()
        .id
}

mod app_channel;
mod context;
mod dispatch_pause;
mod engine_health_report;
mod open_document;
mod pr_status;
mod probe_delivery;
mod proposals;
mod session_sink_queue;
mod t02;
mod t03;
mod t04;
mod t05;
mod t06;
mod t07;
mod trust_authorization;
mod worker_pane_interaction;
mod worker_pane_lifecycle;
mod worker_probe_dispatch;
mod worker_process_reaping;
mod worker_readoption;
mod worker_tier;
