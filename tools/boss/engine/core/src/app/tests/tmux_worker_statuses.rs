//! Coverage for [`ServerState::tmux_worker_statuses`]: one case per
//! [`boss_protocol::TmuxAdoptionState`], plus the tmux-unavailable and
//! `list_sessions`-failure early returns, asserting the exact tmux calls.

use super::tmux_stub::{failure, fake_tmux, ok};
use super::*;
use crate::test_support::*;
use crate::work::WorkDb;
use boss_protocol::TmuxAdoptionState;

fn seed_tmux_run(db: &WorkDb, work_item_id: &str, session_name: &str, token: &str) -> String {
    let execution_id = create_old_execution(db, work_item_id);
    db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert!(
        db.record_tmux_spawn_intent_for_execution(&execution_id, boss_tmux::SERVER_LABEL, session_name, token)
            .unwrap()
    );
    assert!(
        db.record_tmux_session_created_for_execution(&execution_id, token, 4242)
            .unwrap()
    );
    execution_id
}

fn seed_live_tmux_worker(server_state: &ServerState, session_name: &str, token: &str) -> String {
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "tmux status chore");
    let execution_id = seed_tmux_run(db, &work_item_id, session_name, token);
    register_idle_worker(server_state, &execution_id, 3);
    execution_id
}

fn list_sessions_line(session_name: &str) -> String {
    format!("{session_name}\t\n")
}

#[tokio::test]
async fn tmux_unavailable_is_probe_unavailable_and_issues_no_tmux_calls() {
    let (server_state, _dir) = test_server_state();
    let execution_id = seed_live_tmux_worker(&server_state, "boss-1-example", "tok-ours");

    let statuses = server_state.tmux_worker_statuses().await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].execution_id, execution_id);
    assert_eq!(statuses[0].session_name.as_deref(), Some("boss-1-example"));
    assert_eq!(statuses[0].adoption_state, TmuxAdoptionState::ProbeUnavailable);
    assert!(statuses[0].pane_dead.is_none());
    assert!(statuses[0].last_output_at.is_none());
    assert!(statuses[0].attach_command.is_none());
}

#[tokio::test]
async fn not_tmux_hosted_when_the_live_worker_has_no_durable_identity() {
    let (server_state, _dir) = test_server_state();
    register_idle_worker(&server_state, "run-app-hosted", 2);
    let (tmux, runner) = fake_tmux([ok("")]);
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux);

    let statuses = server_state.tmux_worker_statuses().await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].execution_id, "run-app-hosted");
    assert_eq!(statuses[0].adoption_state, TmuxAdoptionState::NotTmuxHosted);
    assert!(statuses[0].session_name.is_none());
    assert!(statuses[0].attach_command.is_none());
    // No durable tmux identity means no tmux call is issued at all — see
    // the doc comment on `tmux_worker_statuses`.
    assert_eq!(runner.calls(), Vec::<Vec<&str>>::new());
}

#[tokio::test]
async fn list_sessions_failure_is_probe_unavailable() {
    let (server_state, _dir) = test_server_state();
    let execution_id = seed_live_tmux_worker(&server_state, "boss-1-example", "tok-ours");
    let (tmux, runner) = fake_tmux([failure("error connecting to server")]);
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux);

    let statuses = server_state.tmux_worker_statuses().await;
    assert_eq!(statuses[0].execution_id, execution_id);
    assert_eq!(statuses[0].adoption_state, TmuxAdoptionState::ProbeUnavailable);
    assert_eq!(statuses[0].session_name.as_deref(), Some("boss-1-example"));
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "list-sessions",
            "-F",
            "#{session_name}\t#{@boss_spawn_token}"
        ]],
    );
}

#[tokio::test]
async fn session_missing_issues_only_list_sessions() {
    let (server_state, _dir) = test_server_state();
    let execution_id = seed_live_tmux_worker(&server_state, "boss-1-example", "tok-ours");
    let (tmux, runner) = fake_tmux([ok("other-session\t\n")]);
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux);

    let statuses = server_state.tmux_worker_statuses().await;
    assert_eq!(statuses[0].execution_id, execution_id);
    assert_eq!(statuses[0].adoption_state, TmuxAdoptionState::SessionMissing);
    assert_eq!(statuses[0].session_name.as_deref(), Some("boss-1-example"));
    assert!(statuses[0].attach_command.is_none());
    assert_eq!(
        runner.calls(),
        vec![vec![
            "-S",
            boss_tmux::TEST_SOCKET_PATH,
            "list-sessions",
            "-F",
            "#{session_name}\t#{@boss_spawn_token}"
        ]],
    );
}

#[tokio::test]
async fn token_mismatch_stops_after_show_environment() {
    let (server_state, _dir) = test_server_state();
    let execution_id = seed_live_tmux_worker(&server_state, "boss-1-example", "tok-ours");
    let (tmux, runner) = fake_tmux([
        ok(&list_sessions_line("boss-1-example")),
        ok("BOSS_SPAWN_TOKEN=tok-someone-elses\n"),
    ]);
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux);

    let statuses = server_state.tmux_worker_statuses().await;
    assert_eq!(statuses[0].execution_id, execution_id);
    assert_eq!(statuses[0].adoption_state, TmuxAdoptionState::TokenMismatch);
    assert!(statuses[0].pane_dead.is_none());
    assert!(statuses[0].attach_command.is_none());
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "list-sessions",
                "-F",
                "#{session_name}\t#{@boss_spawn_token}"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
        ],
    );
}

#[tokio::test]
async fn adopted_live_pane_reads_activity_and_returns_attach_command() {
    let (server_state, _dir) = test_server_state();
    let execution_id = seed_live_tmux_worker(&server_state, "boss-1-example", "tok-ours");
    let (tmux, runner) = fake_tmux([
        ok(&list_sessions_line("boss-1-example")),
        ok("BOSS_SPAWN_TOKEN=tok-ours\n"),
        ok("0"),
        ok("1776528000"),
        ok("claude"),
    ]);
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux);

    let statuses = server_state.tmux_worker_statuses().await;
    assert_eq!(statuses[0].execution_id, execution_id);
    assert_eq!(statuses[0].adoption_state, TmuxAdoptionState::Adopted);
    assert_eq!(statuses[0].pane_dead, Some(false));
    let expected_last_output = crate::live_worker_state::iso8601_utc(1_776_528_000);
    assert_eq!(
        statuses[0].last_output_at.as_deref(),
        Some(expected_last_output.as_str())
    );
    assert_eq!(
        statuses[0].attach_command.as_deref(),
        Some("/opt/homebrew/bin/tmux -L boss attach-session -t boss-1-example")
    );
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "list-sessions",
                "-F",
                "#{session_name}\t#{@boss_spawn_token}"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "display-message",
                "-p",
                "-t",
                "boss-1-example",
                "#{pane_dead}"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "display-message",
                "-p",
                "-t",
                "boss-1-example",
                "#{window_activity}"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "display-message",
                "-p",
                "-t",
                "boss-1-example",
                "#{pane_current_command}"
            ],
        ],
    );
}

#[tokio::test]
async fn adopted_dead_pane_still_reports_last_output_at() {
    let (server_state, _dir) = test_server_state();
    let execution_id = seed_live_tmux_worker(&server_state, "boss-1-example", "tok-ours");
    let (tmux, runner) = fake_tmux([
        ok(&list_sessions_line("boss-1-example")),
        ok("BOSS_SPAWN_TOKEN=tok-ours\n"),
        ok("1"),
        ok("SIGHUP"),
        ok("1776528000"),
    ]);
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux);

    let statuses = server_state.tmux_worker_statuses().await;
    assert_eq!(statuses[0].execution_id, execution_id);
    assert_eq!(statuses[0].adoption_state, TmuxAdoptionState::Adopted);
    assert_eq!(statuses[0].pane_dead, Some(true));
    let expected_last_output = crate::live_worker_state::iso8601_utc(1_776_528_000);
    assert_eq!(
        statuses[0].last_output_at.as_deref(),
        Some(expected_last_output.as_str())
    );
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "list-sessions",
                "-F",
                "#{session_name}\t#{@boss_spawn_token}"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "display-message",
                "-p",
                "-t",
                "boss-1-example",
                "#{pane_dead}"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "display-message",
                "-p",
                "-t",
                "boss-1-example",
                "#{pane_dead_status}"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "display-message",
                "-p",
                "-t",
                "boss-1-example",
                "#{window_activity}"
            ],
        ],
    );
}

#[tokio::test]
async fn token_probe_failure_is_probe_unavailable() {
    let (server_state, _dir) = test_server_state();
    let execution_id = seed_live_tmux_worker(&server_state, "boss-1-example", "tok-ours");
    let (tmux, runner) = fake_tmux([
        ok(&list_sessions_line("boss-1-example")),
        failure("error connecting to server"),
    ]);
    *server_state.pane_delivery_tmux_override.write().unwrap() = Some(tmux);

    let statuses = server_state.tmux_worker_statuses().await;
    assert_eq!(statuses[0].execution_id, execution_id);
    assert_eq!(statuses[0].adoption_state, TmuxAdoptionState::ProbeUnavailable);
    assert_eq!(
        runner.calls(),
        vec![
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "list-sessions",
                "-F",
                "#{session_name}\t#{@boss_spawn_token}"
            ],
            vec![
                "-S",
                boss_tmux::TEST_SOCKET_PATH,
                "show-environment",
                "-t",
                "boss-1-example",
                "BOSS_SPAWN_TOKEN"
            ],
        ],
    );
}
