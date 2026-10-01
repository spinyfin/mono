//! `ServerState::reattach_worker_panes_to_registered_app` — the worker-viewer
//! counterpart of `attach_coordinator_to_registered_app`.
//!
//! `AttachWorkerPane` is sent from exactly one production spawn site, so a
//! run this engine process did not just spawn (a prior app session died, or
//! this engine process readopted the run across its own restart) never gets
//! a viewer unless something re-sends it. These tests drive both the
//! reattach helper and the periodic husk sweep against a real
//! `ServerState`/DB, with scripted tmux inventory for adoption.

use super::*;

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use boss_tmux::{CommandOutput, CommandRunner, Tmux};

use crate::live_worker_state::LiveSpawnRouting;
use crate::test_support::*;

/// A tmux runner that must never actually be invoked: every assertion here
/// only reads `Tmux::socket_path()`, which is a pure getter. A call into
/// this stub means the code under test tried to shell out, which the
/// re-attach path has no business doing.
struct UnusedRunner;

#[async_trait]
impl CommandRunner for UnusedRunner {
    async fn run(&self, _program: &Path, _args: &[OsString], _cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        panic!("worker pane reattach must not shell out to tmux; it only needs the resolved socket path");
    }
}

/// Seed a real, running, tmux-hosted execution and its durable tmux
/// identity, then register a matching live-state entry for `slot_id`.
/// Returns the execution id.
pub(super) fn seed_tmux_hosted_live_run(
    server_state: &ServerState,
    slot_id: u8,
    session_name: &str,
    spawn_token: &str,
) -> String {
    let execution_id = seed_durable_run(server_state, session_name, spawn_token);
    server_state.live_worker_states.register_spawn_with_capabilities(
        slot_id,
        execution_id.clone(),
        "claude-opus-4-7",
        0,
        None,
        true,
        LiveSpawnRouting::new_with_hosting(Some("main".to_owned()), "task_implementation", true),
    );
    execution_id
}

fn seed_durable_run(server_state: &ServerState, session_name: &str, spawn_token: &str) -> String {
    let product_id = create_product(&server_state.work_db);
    let work_item_id = create_active_chore(&server_state.work_db, &product_id, "reattach test chore");
    let db = server_state.work_db.as_ref();
    let execution_id = create_old_execution(db, &work_item_id);
    let (_, run) = db
        .start_execution_run_on_host_with_tmux_hosting(
            &execution_id,
            "worker-1",
            "repo-1",
            "lease-1",
            "ws-1",
            "/tmp/ws",
            "local",
            true,
        )
        .unwrap();
    db.set_run_shell_pid_for_execution(&execution_id, i64::from(std::process::id()))
        .unwrap();
    finish_run_worker_pane_alive(db, &execution_id, &run.id, None);
    assert!(
        server_state
            .work_db
            .record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::SERVER_LABEL, session_name, spawn_token)
            .unwrap(),
        "precondition: the seeded execution must already have a work_runs row to attach identity to",
    );
    assert!(
        server_state
            .work_db
            .record_tmux_session_created_for_execution(&execution_id, spawn_token, 4242)
            .unwrap(),
    );
    execution_id
}

fn install_tmux_override(server_state: &ServerState) {
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(
        Tmux::with_runner_and_socket("/usr/bin/tmux", Arc::new(UnusedRunner), boss_tmux::TEST_SOCKET_PATH).unwrap(),
    );
}

/// Drain the app-bound `ListHostedPanes` request `reattach_worker_panes_to_registered_app`
/// issues first (its dedup query) and answer it with `hosted`.
pub(super) async fn answer_list_hosted_panes(
    server_state: &ServerState,
    sink: &SessionSink,
    hosted: Vec<(String, u8)>,
) {
    let envelope = tokio::time::timeout(Duration::from_secs(10), sink.next())
        .await
        .expect("ListHostedPanes request timed out")
        .expect("ListHostedPanes request");
    let request_id = match &envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request: EngineToAppRequest::ListHostedPanes(_),
        } => request_id.clone(),
        other => panic!("expected ListHostedPanes EngineRequest, got {other:?}"),
    };
    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ListHostedPanes {
                result: Ok(crate::protocol::ListHostedPanesResult {
                    panes: hosted
                        .into_iter()
                        .map(|(run_id, slot_id)| crate::protocol::HostedPaneEntry {
                            run_id,
                            slot_id,
                            summary: None,
                            task_title: None,
                        })
                        .collect(),
                }),
            },
        )
        .await;
}

/// The core fix: a live, tmux-hosted run with no app viewer gets
/// `AttachWorkerPane`, built from durable state (the recorded tmux
/// identity's socket + session name, the live-state slot/run id, and the
/// bound work item's name as `task_title`).
#[tokio::test]
async fn attaches_a_live_tmux_hosted_run_with_no_existing_viewer() {
    let (server_state, _dir) = test_server_state();
    let run_id = seed_tmux_hosted_live_run(&server_state, 3, "boss-3-reattach", "reattach-token-1");
    install_tmux_override(&server_state);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let state = server_state.clone();
    let pass = tokio::spawn(async move {
        state.reattach_worker_panes_to_registered_app().await;
    });

    answer_list_hosted_panes(&server_state, &sink, vec![]).await;

    let envelope = tokio::time::timeout(Duration::from_secs(10), sink.next())
        .await
        .expect("AttachWorkerPane request timed out")
        .expect("AttachWorkerPane request");
    let (request_id, input) = match &envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request: EngineToAppRequest::AttachWorkerPane(input),
        } => (request_id.clone(), input.clone()),
        other => panic!("expected AttachWorkerPane EngineRequest, got {other:?}"),
    };
    assert_eq!(input.run_id, run_id);
    assert_eq!(input.slot_id, 3);
    assert_eq!(input.session_name, "boss-3-reattach");
    assert_eq!(input.tmux_socket_path, boss_tmux::TEST_SOCKET_PATH);

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::AttachWorkerPane {
                result: Ok(crate::protocol::AttachWorkerPaneResult {}),
            },
        )
        .await;

    pass.await.expect("reattach pass task panicked");
}

/// A run the app already hosts a pane for must not receive a second
/// `AttachWorkerPane` — the engine determines this itself from
/// `ListHostedPanes` rather than leaning on the app's own `SlotBusy`
/// refusal as flow control.
#[tokio::test]
async fn skips_a_run_the_app_already_hosts() {
    let (server_state, _dir) = test_server_state();
    let run_id = seed_tmux_hosted_live_run(&server_state, 4, "boss-4-reattach", "reattach-token-2");
    install_tmux_override(&server_state);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let state = server_state.clone();
    let pass = tokio::spawn(async move {
        state.reattach_worker_panes_to_registered_app().await;
    });

    answer_list_hosted_panes(&server_state, &sink, vec![(run_id.clone(), 4)]).await;

    // No further request should follow: draining the sink with a short
    // timeout must find nothing, not an AttachWorkerPane.
    let next = tokio::time::timeout(Duration::from_millis(200), sink.next()).await;
    assert!(
        next.is_err(),
        "must not attach into a slot the app already hosts a live session for, got {next:?}",
    );

    pass.await.expect("reattach pass task panicked");
}

/// Registration ordering: the app registers first (nothing in live-state yet,
/// so its own reattach finds no candidates), then the periodic adoption pass
/// registers a worker. The engine must send that app an attach for it.
#[tokio::test]
async fn attaches_a_worker_registered_by_a_later_adoption_pass() {
    assert_periodic_adoption_attaches(false).await;
}

#[tokio::test]
async fn attaches_a_terminal_handoff_worker() {
    assert_periodic_adoption_attaches(true).await;
}

async fn assert_periodic_adoption_attaches(terminal: bool) {
    let (server_state, _dir) = test_server_state();
    install_tmux_override(&server_state);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let run_id = seed_durable_run(&server_state, "boss-1-periodic", "periodic-token");
    if terminal {
        server_state
            .work_db
            .mark_execution_orphaned(&run_id, "inferred death")
            .unwrap();
    }
    install_inventory(&server_state, true);
    assert!(server_state.live_worker_states.snapshot().is_empty());
    let state = server_state.clone();
    let pass = tokio::spawn(async move {
        use crate::husk_pane_sweep::HuskPaneSweepSource;
        state.list_husk_candidates().await
    });

    answer_list_hosted_panes(&server_state, &sink, vec![]).await;

    let envelope = tokio::time::timeout(Duration::from_secs(10), sink.next())
        .await
        .expect("AttachWorkerPane request timed out")
        .expect("AttachWorkerPane request");
    let (request_id, input) = match &envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request: EngineToAppRequest::AttachWorkerPane(input),
        } => (request_id.clone(), input.clone()),
        other => panic!("expected AttachWorkerPane EngineRequest, got {other:?}"),
    };
    assert_eq!(input.run_id, run_id);
    assert_eq!(input.slot_id, 1);
    assert_eq!(server_state.live_worker_states.get(1).unwrap().run_id, run_id);
    assert_eq!(
        server_state.work_db.get_execution(&run_id).unwrap().status,
        crate::work::ExecutionStatus::Running
    );

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::AttachWorkerPane {
                result: Ok(crate::protocol::AttachWorkerPaneResult {}),
            },
        )
        .await;
    pass.await.expect("reattach pass task panicked");
}

struct InventoryRunner(bool);

#[async_trait]
impl CommandRunner for InventoryRunner {
    async fn run(&self, _program: &Path, args: &[OsString], _cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        let stdout = match args[2].as_ref() {
            "list-sessions" => {
                if self.0 {
                    "boss-1-periodic\t\n".to_owned()
                } else {
                    String::new()
                }
            }
            "show-environment" => match args[5].as_ref() {
                "BOSS_SPAWN_TOKEN" => "BOSS_SPAWN_TOKEN=periodic-token\n".to_owned(),
                "BOSS_SESSION_SCHEMA" => format!("BOSS_SESSION_SCHEMA={}\n", crate::spawn_flow::TMUX_SESSION_SCHEMA),
                other => panic!("unexpected environment query: {other}"),
            },
            "display-message" => match args[6].as_ref() {
                "#{pane_pid}" => format!("{}\n", std::process::id()),
                "#{pane_dead}" => "0\n".to_owned(),
                other => panic!("unexpected pane query: {other}"),
            },
            other => panic!("unexpected tmux command: {other}"),
        };
        Ok(CommandOutput {
            success: true,
            code: Some(0),
            stdout,
            stderr: String::new(),
        })
    }
}

fn install_inventory(state: &ServerState, has_worker: bool) {
    state.set_tmux_override_for_test(
        Tmux::with_runner_and_socket(
            "/usr/bin/tmux",
            Arc::new(InventoryRunner(has_worker)),
            boss_tmux::TEST_SOCKET_PATH,
        )
        .unwrap(),
    );
}

#[tokio::test]
async fn empty_periodic_adoption_sends_no_app_requests() {
    use crate::husk_pane_sweep::HuskPaneSweepSource;
    let (state, _dir) = test_server_state();
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    install_inventory(&state, false);
    assert!(state.list_husk_candidates().await.unwrap().is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn periodic_adoption_without_app_registers_worker_without_requests() {
    use crate::husk_pane_sweep::HuskPaneSweepSource;
    let (state, _dir) = test_server_state();
    let run_id = seed_durable_run(&state, "boss-1-periodic", "periodic-token");
    install_inventory(&state, true);
    // A session sink that is not registered as the app must receive no requests.
    let sink = make_session_sink();
    state.topic_broker.register_session("session-app", sink.clone()).await;
    assert!(state.app_session.lock().await.is_none());
    assert!(state.list_husk_candidates().await.unwrap().is_empty());
    assert_eq!(state.live_worker_states.get(1).unwrap().run_id, run_id);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn repeated_periodic_adoption_sends_no_app_requests_for_existing_worker() {
    use crate::husk_pane_sweep::HuskPaneSweepSource;
    let (state, _dir) = test_server_state();
    seed_tmux_hosted_live_run(&state, 1, "boss-1-periodic", "periodic-token");
    install_inventory(&state, true);
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    assert!(state.list_husk_candidates().await.unwrap().is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err()
    );
}
