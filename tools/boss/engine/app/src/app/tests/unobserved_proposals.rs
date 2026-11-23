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
                state.staged_unobserved_commands.list(&execution_id),
                [command],
                "{arguments}"
            );
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
        boss_engine_completion::StopOutcome::AlreadyTerminal
    );
    assert!(state.pop_pending_probe(&execution_id).is_none());
}

#[tokio::test]
async fn proposal_recovery_survives_hold_and_build_wait_narration() {
    use boss_engine_completion::StopOutcome;
    use boss_engine_completion::tests::{StubPrDetector, TestHarness, write_assistant_transcript};
    use boss_observation_contracts::CompletionRequests;

    let (state, dir, execution_id, _) = live_chore_execution();
    write_assistant_transcript(
        &state.work_db,
        dir.path(),
        &execution_id,
        "Waiting for the build to finish.",
    );
    let TestHarness { handler, probes, .. } = TestHarness::new(state.work_db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_staged_unobserved_commands(state.staged_unobserved_commands.clone())
        .with_hold_registry(state.hold_registry.clone());
    state
        .staged_unobserved_commands
        .record(&execution_id, "boss propose wait --reason building --duration 60");
    state.hold_registry.hold(&execution_id, None, 0);
    assert!(matches!(handler.on_stop(&execution_id).await, StopOutcome::Held { .. }));
    assert!(probes.snapshot().is_empty());
    assert!(state.staged_unobserved_commands.has_unobserved_proposal(&execution_id));
    assert!(handler.pending_background_nudge_execution_ids().contains(&execution_id));
    state.hold_registry.release(&execution_id);
    assert_eq!(
        handler.recheck_background_nudge(&execution_id).await,
        Some(StopOutcome::AwaitingInput)
    );
    assert_eq!(probes.snapshot().len(), 1);
    assert!(probes.snapshot()[0].1.contains("unobserved boss propose"));
    assert!(!state.staged_unobserved_commands.has_unobserved_proposal(&execution_id));
    assert_eq!(state.work_db.execution_run_done_outcome(&execution_id).unwrap(), None);
}

#[tokio::test]
async fn repeated_abandoned_proposals_through_stop_trip_visible_breaker() {
    use boss_engine_completion::tests::{StubPrDetector, TestHarness};
    use boss_engine_completion::{NUDGE_BREAKER_ATTENTION_KIND, StopOutcome};
    let (state, _dir, execution_id, _) = live_chore_execution();
    let TestHarness { handler, probes, .. } = TestHarness::new(state.work_db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_max_unproductive_nudges(2)
        .with_staged_unobserved_commands(state.staged_unobserved_commands.clone());
    for _ in 0..2 {
        state
            .staged_unobserved_commands
            .record(&execution_id, "boss propose done --outcome delivered");
        assert_eq!(handler.on_stop(&execution_id).await, StopOutcome::AwaitingInput);
    }
    assert_eq!(probes.snapshot().len(), 2);
    state
        .staged_unobserved_commands
        .record(&execution_id, "boss propose done --outcome delivered");
    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::NudgeBreakerParked { .. }
    ));
    assert_eq!(
        state.work_db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Failed
    );
    assert!(
        state
            .work_db
            .list_attention_items(&execution_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == NUDGE_BREAKER_ATTENTION_KIND)
    );
}

#[tokio::test]
async fn debounced_proposal_recovery_is_replayed_without_another_stop() {
    use boss_engine_completion::StopOutcome;
    use boss_engine_completion::nudge_breaker::{MIN_RENUDGE_INTERVAL, NudgeBreaker};
    use boss_engine_completion::tests::{StubPrDetector, TestHarness};
    use boss_observation_contracts::CompletionRequests;
    let (state, _dir, execution_id, _) = live_chore_execution();
    let TestHarness { handler, probes, .. } = TestHarness::new(state.work_db.clone(), StubPrDetector::ok(None));
    let instant = std::time::Instant::now();
    let clock = Arc::new(std::sync::Mutex::new(instant));
    let clock_read = clock.clone();
    let breaker = Arc::new(NudgeBreaker::new());
    breaker.record(&execution_id, "codex-unobserved-proposal", 3, instant);
    let handler = handler
        .with_nudge_breaker(breaker)
        .with_now_fn(Arc::new(move || *clock_read.lock().unwrap()))
        .with_staged_unobserved_commands(state.staged_unobserved_commands.clone());
    state
        .staged_unobserved_commands
        .record(&execution_id, "boss propose done --outcome delivered");
    assert_eq!(handler.on_stop(&execution_id).await, StopOutcome::NudgeDebounced);
    assert!(probes.snapshot().is_empty());
    assert!(state.staged_unobserved_commands.has_unobserved_proposal(&execution_id));
    *clock.lock().unwrap() += MIN_RENUDGE_INTERVAL + std::time::Duration::from_secs(1);
    assert_eq!(
        handler.recheck_background_nudge(&execution_id).await,
        Some(StopOutcome::AwaitingInput)
    );
    assert_eq!(probes.snapshot().len(), 1);
}

#[tokio::test]
async fn accepted_review_guide_finalizes_before_proposal_recovery() {
    let (state, _dir) = test_server_state();
    let db = &state.work_db;
    let product = boss_engine_runtime::test_support::create_product(db);
    let root = boss_engine_runtime::test_support::create_active_chore(db, &product, "guide producer");
    let (series, comparison) = boss_engine_runtime::test_support::seed_review_guide_series(db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series, &comparison, boss_review_guide::PROMPT_VERSION)
        .unwrap();
    let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    db.start_execution_run(&execution.id, "review-1", "mono", "lease-1", "ws-1", "/tmp/ws-1")
        .unwrap();
    db.record_execution_launch_config(&execution.id, "codex", "gpt-6-astra", None)
        .unwrap();
    let pid = std::process::id() as libc::pid_t;
    state.worker_registry.register(pid, execution.id.clone());
    let guide = "# Guide\n## Orientation\nA change.\n## Problem\n## Implementation\n## Example\n## Review\n## Evidence\nNo validation results reported; no screenshots registered.";
    // Persist the guide but stop before the handler's finalization step.
    // A live caller must be able to replay this acknowledgement safely.
    let validated =
        boss_engine_proposal_validation::validate_payload(ProposalKind::ReviewGuide, &json!({"body_markdown": guide}))
            .unwrap();
    let key = boss_engine_proposal_validation::derive_idempotency_key(
        &execution.id,
        ProposalKind::ReviewGuide,
        &validated.canonical_json,
    );
    let persisted = boss_engine_runtime::review_guide_validation::submit_worker_proposal_with_flags(
        db,
        boss_work_db::SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &execution.work_item_id,
            kind: ProposalKind::ReviewGuide,
            payload_json: &validated.canonical_json,
            idempotency_key: &key,
        },
        false,
    )
    .unwrap()
    .unwrap();
    assert!(!persisted.already_submitted);
    assert!(db.get_execution(&execution.id).unwrap().status.is_live());

    state
        .staged_unobserved_commands
        .record(&execution.id, "boss propose review-guide");
    state.completion_handler.on_stop(&execution.id).await;
    assert!(db.get_execution(&execution.id).unwrap().status.is_terminal());
    assert!(state.pop_pending_probe(&execution.id).is_none());
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
}
