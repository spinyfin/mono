use super::*;

/// Real rollout normalization, notification staging and completion must queue
/// a recovery prompt even when no proposal ever reached the engine.
#[tokio::test]
async fn backgrounded_proposal_is_reprompted_at_turn_completion() {
    for command in [
        "\"$BOSS_BIN\" propose done --outcome delivered --summary shipped",
        "\"$BOSS_BIN\" propose pr-created --url https://github.com/example/repo/pull/9",
    ] {
        for arguments in [
            json!({"cmd":command,"yield_time_ms":1000}).to_string(),
            format!(
                "{{cmd: {}, yield_time_ms: 1000}}",
                serde_json::to_string(command).unwrap()
            ),
            format!("{{cmd: '{command}', yield_time_ms: 1000,}}"),
        ] {
            for output in [
                "Script running with cell ID 7\nWall time 1 seconds\nOutput:\n",
                "Script completed\nWall time 1 seconds\nOutput:\n{\"session_id\":7861,\"output\":\"\",\"wall_time_seconds\":1.0}",
            ] {
                assert_proposal_recovery(command, &arguments, output).await;
            }
        }
    }
}

async fn assert_proposal_recovery(command: &str, arguments: &str, output: &str) {
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
        json!({"type":"session_meta","payload":{"id":"thread-propose"}}),
        json!({"type":"response_item","payload":{
            "type":"custom_tool_call","name":"exec","call_id":"proposal",
            "input":format!("const r = await tools.exec_command({arguments}); text(r);")
        }}),
        json!({"type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"proposal","output":output
        }}),
    ] {
        session.normalize_progress_events(&envelope).unwrap();
    }
    let events = session
        .normalize_progress_events(&json!({
            "type":"event_msg","payload":{"type":"task_complete","last_agent_message":"All done, PR opened"}
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
                state.staged_unobserved_commands.list(&execution_id),
                [command],
                "{arguments}"
            );
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
    let probe = state
        .pop_pending_probe(&execution_id)
        .expect("must queue recovery on this Stop");
    assert!(probe.text.contains("unobserved boss propose"), "{}", probe.text);
    assert!(probe.text.contains("Poll its existing session"));
    assert_eq!(state.work_db.execution_run_done_outcome(&execution_id).unwrap(), None);
    assert!(state.work_db.get_execution(&execution_id).unwrap().status.is_live());
}

#[tokio::test]
async fn accepted_terminal_proposal_is_not_reprompted() {
    let (state, _dir, execution_id, _) = live_chore_execution();
    let (proposal, _) = submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome":"blocked","summary":"Required credential unavailable"}),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, ProposalState::Applied);
    state
        .staged_unobserved_commands
        .record(&execution_id, "boss propose done --outcome blocked");
    assert_eq!(
        state.completion_handler.on_stop(&execution_id).await,
        crate::completion::StopOutcome::AlreadyTerminal
    );
    assert!(state.pop_pending_probe(&execution_id).is_none());
}
