use super::*;

#[tokio::test]
async fn agy_refused_tools_reject_repeated_no_ops_and_file_bounded_audit_with_overflow() {
    use boss_engine_completion::codex_unobserved_command::{
        MAX_COMMANDS_PER_EXECUTION, UNOBSERVED_COMMAND_ATTENTION_KIND, UNOBSERVED_COMMAND_OVERFLOW_ATTENTION_KIND,
    };
    use boss_engine_driver::{AgentDriver, ProgressSessionConfig};
    let (state, _dir, execution_id, _) = live_chore_execution();
    let driver = boss_engine_driver::agy::AgyDriver::default();
    let mut session = driver.progress_session(&ProgressSessionConfig::default()).unwrap();
    session
        .normalize_progress_events(&json!({"hookEventName": "SessionStart", "conversationId": "parent"}))
        .unwrap();
    for id in 0..=MAX_COMMANDS_PER_EXECUTION {
        session
            .normalize_progress_events(&json!({
                "hookEventName": "PreToolUse", "conversationId": "parent",
                "toolCall": {"id": format!("call-{id}"), "name": "run_command", "args": {}}
            }))
            .unwrap();
    }
    for turn in 0..1 {
        submitted(
            call_with_peer(
                &state,
                Some(std::process::id() as libc::pid_t),
                submit_request(
                    &execution_id,
                    ProposalKind::RunDone,
                    json!({"outcome": "no_changes_needed", "summary": format!("No changes, turn {turn}")}),
                ),
            )
            .await,
        );
        let events = session
            .normalize_progress_events(&json!({
                "hookEventName": "Stop", "conversationId": "parent", "terminationReason": "NO_TOOL_CALL", "fullyIdle": true, "error": ""
            }))
            .unwrap();
        for event in events {
            let incoming = boss_engine_runtime::events_socket::IncomingHookEvent::resolve(
                &driver,
                event,
                Some(execution_id.clone()),
                None,
                None,
            );
            crate::app::worker_events::dispatch_codex_unobserved_command_on_notification(&state, &incoming);
            if let Some(boundary) = incoming.turn_boundary() {
                let expected = boss_engine_completion::StopOutcome::AwaitingInput;
                assert_eq!(
                    state
                        .completion_handler
                        .on_stop_with_turn_end(&execution_id, Some(boundary))
                        .await,
                    expected
                );
            }
        }
        assert!(state.work_db.get_execution(&execution_id).unwrap().status.is_live());
        assert_eq!(state.work_db.execution_run_done_outcome(&execution_id).unwrap(), None);
        let items = state.work_db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            items
                .iter()
                .filter(|item| item.kind == UNOBSERVED_COMMAND_ATTENTION_KIND)
                .count(),
            MAX_COMMANDS_PER_EXECUTION
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| item.kind == UNOBSERVED_COMMAND_OVERFLOW_ATTENTION_KIND)
                .count(),
            1
        );
        assert!(
            items
                .iter()
                .filter(|item| item.kind == UNOBSERVED_COMMAND_ATTENTION_KIND)
                .all(|item| item.body_markdown.contains("declare again"))
        );
    }
    // The gate is read-and-clear: with no new refused tool, the re-declared
    // no-op is accepted rather than dead-ending behind a run-long latch.
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "no_changes_needed", "summary": "No changes, re-declared"}),
            ),
        )
        .await,
    );
    let events = session
        .normalize_progress_events(&json!({
            "hookEventName": "Stop", "conversationId": "parent", "terminationReason": "NO_TOOL_CALL", "fullyIdle": true, "error": ""
        }))
        .unwrap();
    assert!(matches!(events.as_slice(), [boss_protocol::WorkerEvent::Stop { .. }]));
    let incoming = boss_engine_runtime::events_socket::IncomingHookEvent::resolve(
        &driver,
        events.into_iter().next().unwrap(),
        Some(execution_id.clone()),
        None,
        None,
    );
    let boundary = incoming.turn_boundary().unwrap();
    state
        .completion_handler
        .on_stop_with_turn_end(&execution_id, Some(boundary))
        .await;
    assert!(!state.work_db.get_execution(&execution_id).unwrap().status.is_live());
}

fn stage_agy_events(
    state: &Arc<ServerState>,
    execution_id: &str,
    events: impl IntoIterator<Item = boss_protocol::WorkerEvent>,
) {
    let driver = boss_engine_driver::agy::AgyDriver::default();
    for event in events {
        let incoming = boss_engine_runtime::events_socket::IncomingHookEvent::resolve(
            &driver,
            event,
            Some(execution_id.to_owned()),
            state.work_db.transcript_path_for_execution(execution_id).unwrap(),
            None,
        );
        crate::app::worker_events::dispatch_codex_unobserved_command_on_notification(state, &incoming);
    }
}

fn assert_pending_without_latch(
    state: &Arc<ServerState>,
    execution_id: &str,
    session: &dyn boss_engine_driver::ProgressSessionNormalizer,
) {
    let snapshot = session.resume_state().unwrap();
    assert!(
        snapshot["pending_tools"]["calls"]
            .as_object()
            .unwrap()
            .keys()
            .any(|key| key.ends_with("/call_1"))
    );
    assert!(state.staged_unobserved_commands.list(execution_id).is_empty());
    assert!(!state.staged_unobserved_commands.consume_unresolved(execution_id));
}

async fn no_stop_fixture(next: Option<serde_json::Value>) {
    use boss_engine_driver::{AgentDriver, ProgressSessionConfig};
    let (state, _dir, execution_id, _) = live_chore_execution();
    let driver = boss_engine_driver::agy::AgyDriver::default();
    let mut session = driver.progress_session(&ProgressSessionConfig::default()).unwrap();
    session
        .normalize_progress_events(&json!({"hookEventName": "SessionStart", "conversationId": "parent"}))
        .unwrap();
    let pre = session
        .normalize_progress_events(&json!({
            "hookEventName": "PreToolUse", "conversationId": "parent",
            "toolCall": {"id": "call_1", "name": "run_command", "args": {}}
        }))
        .unwrap();
    assert!(matches!(
        pre.as_slice(),
        [boss_protocol::WorkerEvent::PreToolUse { .. }]
    ));
    stage_agy_events(&state, &execution_id, pre);
    assert_pending_without_latch(&state, &execution_id, session.as_ref());
    if let Some(raw) = next {
        let events = session.normalize_progress_events(&raw).unwrap();
        assert!(events.is_empty());
        stage_agy_events(&state, &execution_id, events);
        assert_pending_without_latch(&state, &execution_id, session.as_ref());
    }
}

#[tokio::test]
async fn headless_denial_without_stop_stages_nothing() {
    no_stop_fixture(Some(
        json!({"hookEventName": "PostInvocation", "conversationId": "parent"}),
    ))
    .await;
}

#[tokio::test]
async fn approval_dialog_with_no_further_hook_stages_nothing() {
    no_stop_fixture(None).await;
}

#[tokio::test]
async fn no_stop_boundaries_do_not_stage_or_latch_until_idle_stop() {
    use boss_engine_driver::{AgentDriver, ProgressSessionConfig};
    let (state, _dir, execution_id, _) = live_chore_execution();
    let driver = boss_engine_driver::agy::AgyDriver::default();
    let mut session = driver.progress_session(&ProgressSessionConfig::default()).unwrap();
    session
        .normalize_progress_events(&json!({"hookEventName": "SessionStart", "conversationId": "parent"}))
        .unwrap();
    let pre = session
        .normalize_progress_events(&json!({
            "hookEventName": "PreToolUse", "conversationId": "parent",
            "toolCall": {"id": "call_1", "name": "run_command", "args": {}}
        }))
        .unwrap();
    assert!(matches!(
        pre.as_slice(),
        [boss_protocol::WorkerEvent::PreToolUse { .. }]
    ));
    stage_agy_events(&state, &execution_id, pre);
    assert_pending_without_latch(&state, &execution_id, session.as_ref());
    for raw in [
        json!({"hookEventName": "PostInvocation", "conversationId": "parent"}),
        json!({"hookEventName": "Stop", "conversationId": "parent", "terminationReason": "NO_TOOL_CALL", "fullyIdle": false}),
    ] {
        let events = session.normalize_progress_events(&raw).unwrap();
        assert!(events.is_empty());
        stage_agy_events(&state, &execution_id, events);
        assert_pending_without_latch(&state, &execution_id, session.as_ref());
    }
    let events = session
        .normalize_progress_events(&json!({
            "hookEventName": "Stop", "conversationId": "parent", "terminationReason": "NO_TOOL_CALL", "fullyIdle": true
        }))
        .unwrap();
    assert!(matches!(
        events.first(),
        Some(boss_protocol::WorkerEvent::Notification { .. })
    ));
    stage_agy_events(&state, &execution_id, events);
    assert!(!state.staged_unobserved_commands.list(&execution_id).is_empty());
    assert!(state.staged_unobserved_commands.consume_unresolved(&execution_id));
}

#[tokio::test]
async fn completed_but_failed_empty_error_does_not_latch_or_refuse_no_op() {
    use boss_engine_driver::{AgentDriver, ProgressSessionConfig};
    let (state, _dir, execution_id, _) = live_chore_execution();
    let product_id = match state
        .work_db
        .get_work_item(&state.work_db.get_execution(&execution_id).unwrap().work_item_id)
        .unwrap()
    {
        boss_work_db::WorkItem::Task(task) | boss_work_db::WorkItem::Chore(task) => task.product_id,
        other => panic!("expected chore: {other:?}"),
    };
    state
        .work_db
        .update_work_item(
            &product_id,
            boss_protocol::WorkItemPatch {
                default_driver: Some("agy".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let driver = boss_engine_driver::agy::AgyDriver::default();
    let home = provisioned_agy_containment_root(&driver, &execution_id);
    std::fs::create_dir_all(&home).unwrap();
    let transcript = home.join("completed-failure.jsonl");
    std::fs::write(
        &transcript,
        "{\"status\":\"DONE\",\"content\":\"exit 7\",\"exitCode\":7}\n",
    )
    .unwrap();
    state
        .work_db
        .set_run_transcript_path_if_unset(&execution_id, transcript.to_str().unwrap())
        .unwrap();
    let mut session = driver.progress_session(&ProgressSessionConfig::default()).unwrap();
    session
        .normalize_progress_events(&json!({"hookEventName": "SessionStart", "conversationId": "parent"}))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "hookEventName": "PreToolUse", "conversationId": "parent",
            "toolCall": {"id": "call_1", "name": "run_command", "args": {}}
        }))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "hookEventName": "PostToolUse", "conversationId": "parent", "error": "",
            "toolCall": {"id": "call_1", "name": "run_command", "args": {}}
        }))
        .unwrap();
    let events = session
        .normalize_progress_events(&json!({
            "hookEventName": "Stop", "conversationId": "parent", "terminationReason": "NO_TOOL_CALL", "fullyIdle": true
        }))
        .unwrap();
    assert!(matches!(events.as_slice(), [boss_protocol::WorkerEvent::Stop { .. }]));
    stage_agy_events(&state, &execution_id, events);
    assert!(state.staged_unobserved_commands.list(&execution_id).is_empty());
    assert!(!state.staged_unobserved_commands.consume_unresolved(&execution_id));

    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "no_changes_needed", "summary": "Command failed but completed"}),
            ),
        )
        .await,
    );
    assert!(matches!(
        state.completion_handler.on_stop(&execution_id).await,
        boss_engine_completion::StopOutcome::NoChangesNeeded { .. }
    ));
    assert!(state.work_db.list_attention_items(&execution_id).unwrap().is_empty());
    assert!(state.staged_unobserved_commands.list(&execution_id).is_empty());
    assert!(!state.staged_unobserved_commands.consume_unresolved(&execution_id));
}

#[tokio::test]
async fn following_idle_stop_waits_for_refused_tool_staging_before_no_op() {
    use boss_engine_runtime::events_socket::{HookDecodeQueue, IncomingHookEvent, bind_events_socket};
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    let (state, dir, execution_id, work_item_id) = live_chore_execution();
    let product_id = match state.work_db.get_work_item(&work_item_id).unwrap() {
        boss_work_db::WorkItem::Task(task) | boss_work_db::WorkItem::Chore(task) => task.product_id,
        other => panic!("expected a chore, got {other:?}"),
    };
    state
        .work_db
        .update_work_item(
            &product_id,
            boss_protocol::WorkItemPatch {
                default_driver: Some("agy".into()),
                ..Default::default()
            },
        )
        .unwrap();

    let registry = std::sync::Arc::new(boss_engine_driver::DriverRegistry::default());
    let sock = dir.path().join("events.sock");
    let listener = bind_events_socket(&sock).unwrap();
    let mut queue = HookDecodeQueue::default();
    let db = state.work_db.clone();

    async fn send(
        listener: &tokio::net::UnixListener,
        path: &std::path::Path,
        mut payload: serde_json::Value,
        run_id: &str,
    ) -> tokio::net::UnixStream {
        payload["_boss_run_id"] = serde_json::json!(run_id);
        let mut client = UnixStream::connect(path).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        client.write_all(payload.to_string().as_bytes()).await.unwrap();
        client.shutdown().await.unwrap();
        stream
    }

    for payload in [
        json!({"hookEventName": "SessionStart", "conversationId": "parent"}),
        json!({"hookEventName": "PreToolUse", "conversationId": "parent",
            "toolCall": {"id": "call", "name": "run_command", "args": {}}}),
    ] {
        let stream = send(&listener, &sock, payload, &execution_id).await;
        queue.reserve().handle(stream, &registry, &db).await.unwrap();
    }

    let stream = send(
        &listener,
        &sock,
        json!({"hookEventName": "Stop", "conversationId": "parent", "terminationReason": "NO_TOOL_CALL", "fullyIdle": true}),
        &execution_id,
    )
    .await;
    let mut first_turn = queue.reserve();
    let first = first_turn
        .handle(stream, &registry, &db)
        .await
        .unwrap()
        .unwrap()
        .into_ordered_events();
    assert_eq!(first.len(), 2);

    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "no_changes_needed", "summary": "Nothing to do"}),
            ),
        )
        .await,
    );

    let stream = send(
        &listener,
        &sock,
        json!({
            "hookEventName": "Stop", "conversationId": "parent", "terminationReason": "NO_TOOL_CALL", "fullyIdle": true,
            "_boss_delivery_id": "second-idle-stop"
        }),
        &execution_id,
    )
    .await;
    let mut second_turn = queue.reserve();
    let second_db = db.clone();
    let second_registry = registry.clone();
    let second_task = tokio::spawn(async move { second_turn.handle(stream, &second_registry, &second_db).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!second_task.is_finished());

    for event in &first {
        crate::app::worker_events::dispatch_codex_unobserved_command_on_notification(&state, event);
    }
    drop(first_turn);

    let second = second_task.await.unwrap().unwrap().unwrap();
    assert!(matches!(second.event, boss_protocol::WorkerEvent::Stop { .. }));
    let driver = boss_engine_driver::agy::AgyDriver::default();
    let boundary = IncomingHookEvent::resolve(&driver, second.event, Some(execution_id.clone()), None, None);
    assert_eq!(
        state
            .completion_handler
            .on_stop_with_turn_end(&execution_id, boundary.turn_boundary())
            .await,
        boss_engine_completion::StopOutcome::AwaitingInput
    );
    assert_eq!(state.work_db.execution_run_done_outcome(&execution_id).unwrap(), None);
}

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
        boss_engine_completion::StopOutcome::NoChangesNeeded { .. }
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
        boss_engine_completion::StopOutcome::AwaitingInput
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
        boss_engine_completion::StopOutcome::NoChangesNeeded { .. }
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
    use boss_engine_driver::{AgentDriver, CodexDriver, ProgressSessionConfig, ProgressStreamSource};
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
        let incoming = boss_engine_runtime::events_socket::IncomingHookEvent::resolve(
            &driver,
            event,
            Some(execution_id.clone()),
            None,
            None,
        );
        crate::app::worker_events::dispatch_codex_unobserved_command_on_notification(&state, &incoming);
        if let Some(boundary) = incoming.turn_boundary() {
            boundary_seen = true;
            assert_eq!(
                state
                    .completion_handler
                    .on_stop_with_turn_end(&execution_id, Some(boundary))
                    .await,
                boss_engine_completion::StopOutcome::AwaitingInput
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

    // A later turn with no new unobserved command, and a fresh declaration,
    // must close: proposal acceptance is not task completion, but a verified
    // no-op after the refusal is consumed is.
    submitted(
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
    session
        .normalize_progress_events(&json!({"type": "event_msg", "payload": {"type": "task_started"}}))
        .unwrap();
    let later = session
        .normalize_progress_events(&json!({
            "type": "event_msg", "payload": {"type": "task_complete"}
        }))
        .unwrap();
    let mut completed = false;
    for event in later {
        let incoming = boss_engine_runtime::events_socket::IncomingHookEvent::resolve(
            &driver,
            event,
            Some(execution_id.clone()),
            None,
            None,
        );
        crate::app::worker_events::dispatch_codex_unobserved_command_on_notification(&state, &incoming);
        if let Some(boundary) = incoming.turn_boundary() {
            assert!(matches!(
                state
                    .completion_handler
                    .on_stop_with_turn_end(&execution_id, Some(boundary))
                    .await,
                boss_engine_completion::StopOutcome::NoChangesNeeded { .. }
            ));
            completed = true;
        }
    }
    assert!(completed);
    assert_eq!(
        state.work_db.get_execution(&execution_id).unwrap().status,
        boss_protocol::ExecutionStatus::Completed
    );
}

#[tokio::test]
async fn a_completed_functions_exec_wrapper_does_not_block_no_op_completion() {
    use boss_engine_driver::{AgentDriver, CodexDriver, ProgressSessionConfig, ProgressStreamSource};
    let (state, _dir, execution_id, work_item_id) = live_chore_execution();
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
            "type": "custom_tool_call", "name": "functions.exec", "call_id": "validation",
            "input": concat!(
                r#"let r = await tools.exec_command({cmd: "bazel test //tools/boss/engine/codex-rollout:codex-rollout_test", yield_time_ms: 30000});"#,
                r#"while (!("exit_code" in r)) { r = await tools.write_stdin({session_id: r.session_id, chars: ""}); }"#,
                "text(JSON.stringify(r));"
            )
        }}),
        json!({"type": "response_item", "payload": {
            "type": "custom_tool_call_output", "call_id": "validation",
            "output": [
                {"type":"input_text","text":"Script completed\nWall time 17.7 seconds\nOutput:\n"},
                {"type":"input_text","text":"{\"chunk_id\":\"done\",\"session_id\":8467,\"exit_code\":0,\"output\":\"2 tests passed\\n\"}"}
            ]
        }}),
    ] {
        session.normalize_progress_events(&envelope).unwrap();
    }
    submitted(
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
    let events = session
        .normalize_progress_events(&json!({
            "type": "event_msg", "payload": {"type": "task_complete"}
        }))
        .unwrap();
    assert!(
        !events.iter().any(
            |event| matches!(event, boss_protocol::WorkerEvent::Notification { message, .. }
            if message.contains("[codex-unobserved-command]"))
        ),
        "completed wrapper must not emit an unobserved-command notification: {events:?}"
    );
    for event in events {
        let incoming = boss_engine_runtime::events_socket::IncomingHookEvent::resolve(
            &driver,
            event,
            Some(execution_id.clone()),
            None,
            None,
        );
        crate::app::worker_events::dispatch_codex_unobserved_command_on_notification(&state, &incoming);
        if let Some(boundary) = incoming.turn_boundary() {
            assert!(matches!(
                state
                    .completion_handler
                    .on_stop_with_turn_end(&execution_id, Some(boundary))
                    .await,
                boss_engine_completion::StopOutcome::NoChangesNeeded { .. }
            ));
        }
    }
    assert_eq!(
        state.work_db.get_execution(&execution_id).unwrap().status,
        boss_protocol::ExecutionStatus::Completed
    );
    assert_eq!(task_status(&state, &work_item_id), boss_protocol::TaskStatus::Done);
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
        boss_engine_completion::StopOutcome::AwaitingInput
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
            .record_declared_worker_no_op_completion(&execution_id, &old.id, None, "stale declaration", None)
            .unwrap()
            .is_none()
    );
    let (current, _) = submitted(call_with_peer(&state, Some(std::process::id() as libc::pid_t), request()).await);
    assert_ne!(old.id, current.id);
    assert!(
        state
            .work_db
            .record_declared_worker_no_op_completion(&execution_id, &old.id, None, "superseded declaration", None)
            .unwrap()
            .is_none()
    );
    assert!(state.work_db.get_execution(&execution_id).unwrap().status.is_live());
    assert!(
        state
            .work_db
            .record_declared_worker_no_op_completion(&execution_id, &current.id, None, "current declaration", None)
            .unwrap()
            .is_some()
    );
    assert!(
        state
            .work_db
            .record_declared_worker_no_op_completion(&execution_id, &current.id, None, "replayed declaration", None)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn hook_accept_overflow_is_counted_once_and_does_not_rearm_after_staging() {
    use boss_engine_completion::codex_unobserved_command::MAX_COMMANDS_PER_EXECUTION;
    use boss_engine_runtime::events_socket::{HookDecodeQueue, IncomingHookEvent};
    let (state, _dir, execution_id, _) = live_chore_execution();
    for id in 0..MAX_COMMANDS_PER_EXECUTION {
        state
            .staged_unobserved_commands
            .record(&execution_id, &format!("command-{id}"));
    }
    let incoming = IncomingHookEvent::resolve(
        &boss_engine_driver::CodexDriver::default(),
        boss_protocol::WorkerEvent::Notification {
            session_id: "parent".into(),
            message: format!(
                "{}over-cap command",
                boss_engine_driver::codex::UNOBSERVED_COMMAND_MARKER
            ),
        },
        Some(execution_id.clone()),
        None,
        None,
    );
    let mut queue = HookDecodeQueue::default();
    super::super::events_accept::dispatch_hook_events(&state, incoming.clone(), queue.reserve()).await;
    assert_eq!(
        state.metrics.counter_value("codex.unobserved_command_overflow"),
        Some(1)
    );
    assert!(state.staged_unobserved_commands.consume_unresolved(&execution_id));
    // Model a Stop consuming Codex's flag between staging and native fanout.
    super::super::worker_events::dispatch_staged_worker_event_fanout(&state, &incoming).await;
    assert!(!state.staged_unobserved_commands.consume_unresolved(&execution_id));
    assert_eq!(
        state.metrics.counter_value("codex.unobserved_command_overflow"),
        Some(1)
    );
}

/// Drives a refused agy tool through the real wiring (`transcript_path_for_execution` →
/// `driver_for_execution` → containment root → transcript context) and returns the
/// filed attention body.
async fn refused_tool_attention_body(transcript_in_process_home: bool) -> String {
    use boss_engine_completion::codex_unobserved_command::UNOBSERVED_COMMAND_ATTENTION_KIND;
    use boss_engine_driver::{AgentDriver, ProgressSessionConfig};
    let (state, dir, execution_id, _) = live_chore_execution();
    let product_id = match state
        .work_db
        .get_work_item(&state.work_db.get_execution(&execution_id).unwrap().work_item_id)
        .unwrap()
    {
        boss_work_db::WorkItem::Task(task) | boss_work_db::WorkItem::Chore(task) => task.product_id,
        other => panic!("expected chore: {other:?}"),
    };
    state
        .work_db
        .update_work_item(
            &product_id,
            boss_protocol::WorkItemPatch {
                default_driver: Some("agy".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let driver = boss_engine_driver::agy::AgyDriver::default();
    let dir_for_transcript = if transcript_in_process_home {
        provisioned_agy_containment_root(&driver, &execution_id)
    } else {
        dir.path().join("outside-process-home")
    };
    std::fs::create_dir_all(&dir_for_transcript).unwrap();
    let transcript = dir_for_transcript.join("refused.jsonl");
    std::fs::write(
        &transcript,
        "{\"status\":\"ERROR\",\"error\":\"blocked by deny rule: run_command\"}\n",
    )
    .unwrap();
    state
        .work_db
        .set_run_transcript_path_if_unset(&execution_id, transcript.to_str().unwrap())
        .unwrap();
    let mut session = driver.progress_session(&ProgressSessionConfig::default()).unwrap();
    session
        .normalize_progress_events(&json!({"hookEventName": "SessionStart", "conversationId": "parent"}))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "hookEventName": "PreToolUse", "conversationId": "parent",
            "toolCall": {"id": "call_1", "name": "run_command", "args": {}}
        }))
        .unwrap();
    let events = session
        .normalize_progress_events(&json!({
            "hookEventName": "Stop", "conversationId": "parent", "terminationReason": "NO_TOOL_CALL", "fullyIdle": true
        }))
        .unwrap();
    stage_agy_events(&state, &execution_id, events);
    state.completion_handler.on_stop(&execution_id).await;
    state
        .work_db
        .list_attention_items(&execution_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == UNOBSERVED_COMMAND_ATTENTION_KIND)
        .expect("refused-tool attention item")
        .body_markdown
}

#[tokio::test]
async fn refused_tool_attention_carries_contained_transcript_error_reason() {
    let body = refused_tool_attention_body(true).await;
    assert!(body.contains("status: ERROR"), "{body}");
    assert!(body.contains("blocked by deny rule: run_command"), "{body}");
}

#[tokio::test]
async fn refused_tool_attention_reports_uncontained_transcript_as_unavailable() {
    let body = refused_tool_attention_body(false).await;
    assert!(body.contains("Transcript corroboration unavailable"), "{body}");
    assert!(!body.contains("blocked by deny rule"), "{body}");
}

/// The directory an agy run's transcripts must stay under: its durable
/// transcript store. Containment only exists once that store does, so create
/// it the way provisioning would before asking the driver to resolve it.
fn provisioned_agy_containment_root(driver: &boss_engine_driver::agy::AgyDriver, run_id: &str) -> std::path::PathBuf {
    let store = boss_engine_driver::transcript_store::transcript_store_root().unwrap();
    let durable = boss_engine_driver::transcript_store::durable_sessions_dir(&store, "agy", run_id).unwrap();
    std::fs::create_dir_all(durable).unwrap();
    driver.transcript_containment_root(run_id).unwrap().unwrap()
}
