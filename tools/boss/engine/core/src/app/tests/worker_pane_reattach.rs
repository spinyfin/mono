//! `ServerState::reattach_worker_panes_to_registered_app` — the worker-viewer
//! counterpart of `attach_coordinator_to_registered_app`.
//!
//! `AttachWorkerPane` is sent from exactly one production spawn site, so a
//! run this engine process did not just spawn (a prior app session died, or
//! this engine process readopted the run across its own restart) never gets
//! a viewer unless something re-sends it. These tests drive the re-send
//! directly against a real `ServerState`/DB rather than through the
//! `RegisterAppSession` RPC, so they pin the re-attach logic itself: which
//! runs qualify, what identity is sent, and that a slot the app already
//! hosts is left alone.

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
fn seed_tmux_hosted_live_run(server_state: &ServerState, slot_id: u8, session_name: &str, spawn_token: &str) -> String {
    let product_id = create_product(&server_state.work_db);
    let work_item_id = create_active_chore(&server_state.work_db, &product_id, "reattach test chore");
    let execution_id = create_spawned_execution(&server_state.work_db, &work_item_id, 4242);
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

fn install_tmux_override(server_state: &ServerState) {
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(
        Tmux::with_runner_and_socket("/usr/bin/tmux", Arc::new(UnusedRunner), boss_tmux::TEST_SOCKET_PATH).unwrap(),
    );
}

/// Drain the app-bound `ListHostedPanes` request `reattach_worker_panes_to_registered_app`
/// issues first (its dedup query) and answer it with `hosted`.
async fn answer_list_hosted_panes(server_state: &ServerState, sink: &SessionSink, hosted: Vec<(String, u8)>) {
    let envelope = sink.next().await.expect("ListHostedPanes request");
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

    let envelope = sink.next().await.expect("AttachWorkerPane request");
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
