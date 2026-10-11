use super::*;

/// A declaration remains live until the driver makes command evidence authoritative.
#[tokio::test]
async fn accepted_run_done_no_changes_needed_waits_for_stop() {
    let (server_state, _dir, execution_id, work_item_id) = live_chore_execution();
    let peer_pid = std::process::id() as libc::pid_t;

    let (proposal, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "no_changes_needed", "summary": "Already fixed on main"}),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, ProposalState::Applied);
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_live()
    );
    assert!(matches!(
        server_state.completion_handler.on_stop(&execution_id).await,
        crate::completion::StopOutcome::NoChangesNeeded { .. }
    ));

    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        boss_protocol::ExecutionStatus::Completed
    );
    assert_eq!(
        task_status(&server_state, &work_item_id),
        boss_protocol::TaskStatus::Done
    );
}

/// The seam-on declaration waits for the Stop boundary: a `no-changes-needed` declaration made while a Codex
/// command is unobserved is refused (run stays live, declaration consumed),
/// and an identical fresh declaration afterwards is accepted.
#[tokio::test]
async fn run_done_no_changes_needed_is_refused_at_stop_after_an_unobserved_command() {
    let (server_state, _dir, execution_id, work_item_id) = live_chore_execution();
    let peer_pid = std::process::id() as libc::pid_t;
    server_state
        .staged_unobserved_commands
        .record(&execution_id, "bazel test //tools/boss/...");
    let payload = json!({"outcome": "no_changes_needed", "summary": "Already fixed on main"});

    submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(&execution_id, ProposalKind::RunDone, payload.clone()),
        )
        .await,
    );
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_live(),
        "a refused no-op declaration must leave the run live"
    );
    assert_ne!(
        task_status(&server_state, &work_item_id),
        boss_protocol::TaskStatus::Done
    );
    assert_eq!(
        server_state.completion_handler.on_stop(&execution_id).await,
        crate::completion::StopOutcome::AwaitingInput
    );
    assert_eq!(
        server_state.work_db.execution_run_done_outcome(&execution_id).unwrap(),
        None,
        "the refused declaration must be consumed"
    );

    // Declaring again (identical payload) is a fresh submission, and the
    // unobserved flag was consumed by the refusal, so it is accepted.
    let (proposal, already_submitted) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(&execution_id, ProposalKind::RunDone, payload),
        )
        .await,
    );
    assert!(!already_submitted);
    assert_eq!(proposal.state, ProposalState::Applied);
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_live()
    );
    assert!(matches!(
        server_state.completion_handler.on_stop(&execution_id).await,
        crate::completion::StopOutcome::NoChangesNeeded { .. }
    ));

    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        boss_protocol::ExecutionStatus::Completed
    );
    assert_eq!(
        task_status(&server_state, &work_item_id),
        boss_protocol::TaskStatus::Done
    );
}

/// Exercise actual rollout normalization and notification staging, with the
/// proposal submitted before the terminal envelope exposes abandonment.
#[tokio::test]
async fn yielded_codex_command_blocks_a_same_turn_no_op_proposal() {
    use crate::driver::{AgentDriver, CodexDriver, ProgressSessionConfig, ProgressStreamSource};
    let (state, _dir, execution_id, _) = live_chore_execution();
    let driver = CodexDriver::default();
    let mut session = driver
        .progress_session(&ProgressSessionConfig {
            source: ProgressStreamSource::AgentJsonlFile,
            ..Default::default()
        })
        .unwrap();
    for envelope in [
        json!({"type": "session_meta", "payload": {"id": "thread-no-op"}}),
        json!({"type": "response_item", "payload": {
            "type": "custom_tool_call", "name": "exec", "call_id": "validation",
            "input": "const r = await tools.exec_command({cmd: \"bazel test //tools/boss/engine/...\"}); text(r);"
        }}),
        json!({"type": "response_item", "payload": {
            "type": "custom_tool_call_output", "call_id": "validation",
            "output": "Script running with cell ID 7\nWall time 1 seconds\nOutput:\n"
        }}),
    ] {
        session.normalize_progress_events(&envelope).unwrap();
    }
    let (proposal, _) = submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "no_changes_needed", "summary": "Already fixed"}),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, ProposalState::Applied);
    assert!(
        state.work_db.get_execution(&execution_id).unwrap().status.is_live(),
        "submission must not close before command evidence reaches the engine"
    );

    let events = session
        .normalize_progress_events(&json!({
            "type": "event_msg", "payload": {"type": "task_complete"}
        }))
        .unwrap();
    let mut boundary_seen = false;
    for event in events {
        let incoming =
            crate::events_socket::IncomingHookEvent::resolve(&driver, event, Some(execution_id.clone()), None, None);
        crate::app::worker_events::dispatch_codex_unobserved_command_on_notification(&state, &incoming);
        if let Some(boundary) = incoming.turn_boundary() {
            boundary_seen = true;
            assert_eq!(
                state
                    .completion_handler
                    .on_stop_with_turn_end(&execution_id, Some(boundary))
                    .await,
                crate::completion::StopOutcome::AwaitingInput
            );
        }
    }
    assert!(boundary_seen);
    assert!(state.work_db.get_execution(&execution_id).unwrap().status.is_live());
    assert_eq!(state.work_db.execution_run_done_outcome(&execution_id).unwrap(), None);
    assert_eq!(
        state.work_db.get_worker_proposal(&proposal.id).unwrap().state,
        ProposalState::Superseded
    );
}

/// Hold both original and replay RPCs at response delivery, let Stop consume
/// their declaration, then release both finalizers concurrently.
#[tokio::test]
async fn delayed_and_replayed_submit_cannot_revive_a_refused_no_op() {
    let (state, _dir, execution_id, _) = live_chore_execution();
    let sink = make_session_sink();
    let replay_sink = make_session_sink();
    let request = || {
        submit_request(
            &execution_id,
            ProposalKind::RunDone,
            json!({"outcome": "no_changes_needed", "summary": "Already fixed"}),
        )
    };
    let first = proposals::handle_submit_proposal(
        dispatch_with_peer(&state, &sink, Some(std::process::id() as libc::pid_t)),
        request(),
    );
    let replay = proposals::handle_submit_proposal(
        dispatch_with_peer(&state, &replay_sink, Some(std::process::id() as libc::pid_t)),
        request(),
    );
    tokio::pin!(first, replay);
    let ack = tokio::select! {
        ack = sink.next() => ack.unwrap(),
        _ = &mut first => panic!("original handler must wait for delivery"),
    };
    let replay_ack = tokio::select! {
        ack = replay_sink.next() => ack.unwrap(),
        _ = &mut replay => panic!("replay handler must wait for delivery"),
    };
    let (proposal, _) = submitted(ack.payload.clone());
    let (replayed, already_submitted) = submitted(replay_ack.payload.clone());
    assert!(already_submitted);
    assert_eq!(proposal.id, replayed.id);
    state
        .staged_unobserved_commands
        .record(&execution_id, "bazel test //tools/boss/engine/...");
    assert_eq!(
        state.completion_handler.on_stop(&execution_id).await,
        crate::completion::StopOutcome::AwaitingInput
    );
    assert_eq!(
        state.work_db.get_worker_proposal(&proposal.id).unwrap().state,
        ProposalState::Superseded
    );
    assert_eq!(state.work_db.execution_run_done_outcome(&execution_id).unwrap(), None);
    sink.complete_response_delivery(ack.request_id.as_deref(), true);
    replay_sink.complete_response_delivery(replay_ack.request_id.as_deref(), true);
    tokio::join!(first, replay);
    assert!(
        state.work_db.get_execution(&execution_id).unwrap().status.is_live(),
        "neither stale finalizer may authorize no-op completion"
    );
    assert_eq!(state.work_db.execution_run_done_outcome(&execution_id).unwrap(), None);
}

#[tokio::test]
async fn terminal_write_requires_the_exact_current_no_op_proposal() {
    let (state, _dir, execution_id, _) = live_chore_execution();
    let request = || {
        submit_request(
            &execution_id,
            ProposalKind::RunDone,
            json!({"outcome": "no_changes_needed", "summary": "Already fixed"}),
        )
    };
    let (old, _) = submitted(call_with_peer(&state, Some(std::process::id() as libc::pid_t), request()).await);
    state
        .work_db
        .clear_no_changes_needed_declaration(&execution_id)
        .unwrap();
    assert!(
        state
            .work_db
            .record_declared_worker_no_op_completion(&execution_id, &old.id, "stale declaration", None)
            .unwrap()
            .is_none()
    );
    let (current, _) = submitted(call_with_peer(&state, Some(std::process::id() as libc::pid_t), request()).await);
    assert_ne!(old.id, current.id);
    assert!(
        state
            .work_db
            .record_declared_worker_no_op_completion(&execution_id, &old.id, "superseded declaration", None)
            .unwrap()
            .is_none()
    );
    assert!(state.work_db.get_execution(&execution_id).unwrap().status.is_live());
    assert!(
        state
            .work_db
            .record_declared_worker_no_op_completion(&execution_id, &current.id, "current declaration", None)
            .unwrap()
            .is_some()
    );
    assert!(
        state
            .work_db
            .record_declared_worker_no_op_completion(&execution_id, &current.id, "replayed declaration", None)
            .unwrap()
            .is_none()
    );
}
