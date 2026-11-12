//! A probe typed into a parked pane is delivered when the driver starts a
//! turn on it, not when tmux accepts the keys.
//!
//! The defect these pin: a write into a parked Codex pane was recorded
//! `consumed` on the strength of `send-keys` succeeding. The text sat in the
//! composer, no turn started, and the probe held the run's delivery slot for
//! good. The engine's nudges queued behind it and were counted as they were
//! queued, so the worker was failed for ignoring three nudges of which it had
//! been shown none.

use super::*;

use crate::app::parked_delivery::{RETURN_RETRIES, TURN_START_CONFIRM_TIMEOUT};
use crate::app::probes::ServerStateProbeQueuer;
use crate::app::worker_events::dispatch_completion_on_stop;
use boss_engine_completion::nudge_breaker::{ABSOLUTE_MAX_NUDGES, NudgeBreaker};
use boss_engine_completion::tests::{StubPrDetector, TestHarness};
use boss_engine_completion::{NudgeDelivery, ProbeQueuer, StopOutcome};
use boss_observation_contracts::CompletionRequests;
use boss_protocol::WorkerEvent;

const SLOT: u8 = 6;

/// The whole of one delivery attempt: every Return, and slack for the poll.
fn one_failed_delivery() -> Duration {
    TURN_START_CONFIRM_TIMEOUT * (RETURN_RETRIES + 1) + Duration::from_secs(5)
}

fn parked_codex_worker(server_state: &ServerState) -> (String, std::sync::Arc<super::tmux_stub::RecordingPaneRunner>) {
    let run_id = register_idle_worker_with_driver(server_state, SLOT, Some("codex"));
    let runner = install_live_tmux_delivery(server_state, &run_id, SLOT, "boss-6");
    (run_id, runner)
}

fn hook(event: WorkerEvent, run_id: &str) -> boss_engine_runtime::events_socket::IncomingHookEvent {
    boss_engine_runtime::events_socket::IncomingHookEvent::for_test(event, Some(run_id.to_owned()), None)
}

fn stop(run_id: &str) -> boss_engine_runtime::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::Stop {
            session_id: "sess".into(),
            stop_hook_active: false,
            stop_reason: boss_protocol::StopReason::Completed,
        },
        run_id,
    )
}

/// What a driver reports when it takes the composer's contents as a prompt.
/// The prompt identifies the probe whose submission is being confirmed.
fn turn_started(run_id: &str) -> boss_engine_runtime::events_socket::IncomingHookEvent {
    hook(
        WorkerEvent::UserPromptSubmit {
            session_id: "sess".into(),
            prompt: "push your branch".to_owned(),
        },
        run_id,
    )
}

fn undelivered_probe_attentions(server_state: &ServerState, run_id: &str) -> Vec<boss_protocol::WorkAttentionItem> {
    server_state
        .work_db
        .list_attention_items(run_id)
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == crate::app::probes::PROBE_UNDELIVERED_ATTENTION_KIND)
        .collect()
}

/// The write alone settles nothing. The probe is delivered when the driver
/// starts a turn, and only then.
#[tokio::test(start_paused = true)]
async fn a_parked_write_is_delivered_when_the_driver_starts_a_turn() {
    let (server_state, _dir) = test_server_state();
    let (run_id, runner) = parked_codex_worker(&server_state);
    let probe_id = server_state.queue_probe(run_id.clone(), "push your branch".into(), false);

    let outcome = dispatch_probe_now(&server_state, &run_id).await;

    assert_eq!(outcome, ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Injected));
    assert!(runner.wrote_text());
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Injected),
        "tmux accepting the keys is not the driver taking the prompt",
    );

    dispatch_live_worker_state(&server_state, &turn_started(&run_id)).await;

    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Consumed),
    );
    let record = server_state.probe_record(&probe_id).unwrap();
    assert_eq!(record.resumed, Some(ProbeResumeEvidence::Confirmed));

    // A confirmed delivery is left alone: no further Return, no escalation.
    tokio::time::sleep(one_failed_delivery()).await;
    assert_eq!(runner.return_presses(), 1);
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Consumed),
    );
    assert!(undelivered_probe_attentions(&server_state, &run_id).is_empty());
}

/// The defect itself: a write into a parked Codex pane that starts no turn is
/// a failed delivery. It is retried with Return alone, then settled as
/// unconfirmed, surfaced with what the pane showed, and the run's delivery
/// slot is given back.
#[tokio::test(start_paused = true)]
async fn a_parked_codex_write_that_starts_no_turn_is_a_failed_delivery() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, SLOT, Some("codex"));
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, SLOT, "boss-6");
    register_tmux_identity_for_test(&server_state, &run_id, "boss-6", super::tmux_stub::TEST_SPAWN_TOKEN);
    // The composer still holds the text: the Return was taken as a newline.
    let runner = std::sync::Arc::new(
        super::tmux_stub::RecordingPaneRunner::new("boss-6").capture_text("› push your branch\n\n  gpt-6-astra"),
    );
    *server_state.pane_delivery_tmux_override.write().unwrap() =
        Some(super::tmux_stub::tmux_with_runner(runner.clone()));
    let queuer = ServerStateProbeQueuer::default();
    queuer.set_server_state(Arc::downgrade(&server_state));
    queuer.queue_probe(&run_id, "push your branch");
    let probe_id = server_state.in_flight_probe_id(&run_id);
    assert!(probe_id.is_none(), "queued, not yet written");

    let outcome = dispatch_probe_now(&server_state, &run_id).await;
    assert_eq!(outcome, ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Injected));
    let probe_id = server_state
        .in_flight_probe_id(&run_id)
        .expect("the write holds the slot");
    assert_eq!(runner.return_presses(), 1);
    assert_eq!(
        queuer.nudge_delivery(&run_id),
        NudgeDelivery::Pending {
            nudge_id: probe_id.clone()
        },
        "a nudge that started no turn yet has not been shown to the worker",
    );

    // One window with no turn: Return is pressed again, and nothing else.
    tokio::time::sleep(TURN_START_CONFIRM_TIMEOUT + Duration::from_secs(1)).await;
    assert_eq!(runner.return_presses(), 2);
    assert_eq!(
        runner
            .calls()
            .iter()
            .filter(|call| call.iter().any(|arg| arg == "-l"))
            .count(),
        1,
        "the text is typed exactly once; the retry cannot deliver a second copy",
    );
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Injected),
    );

    tokio::time::sleep(one_failed_delivery()).await;

    assert_eq!(runner.return_presses(), 1 + RETURN_RETRIES as usize);
    let record = server_state.probe_record(&probe_id).unwrap();
    assert_eq!(
        record.state,
        ProbeDeliveryState::Unconfirmed,
        "a write that started no turn must not be recorded as consumed",
    );
    let detail = record.detail.expect("the failed delivery must say what happened");
    assert!(detail.contains("the driver never started a turn"), "{detail}");
    assert!(
        detail.contains("pane_in_mode=false") && detail.contains("activity=idle"),
        "the pane's state at the failure must be on the record: {detail}",
    );
    assert!(
        !server_state.has_in_flight_probe(&run_id),
        "a probe that will never be answered must not hold the run's delivery slot",
    );
    let attentions = undelivered_probe_attentions(&server_state, &run_id);
    assert_eq!(attentions.len(), 1, "a failed delivery must be surfaced");
    assert!(
        attentions[0].body_markdown.contains("› push your branch"),
        "the pane contents are what make the next occurrence diagnosable: {}",
        attentions[0].body_markdown,
    );
    assert!(
        matches!(
            queuer.nudge_delivery(&run_id),
            NudgeDelivery::Failed { ref nudge_id, ref detail }
                if nudge_id == &probe_id && detail.contains("never started a turn")
        ),
        "the ladder must learn the nudge was not delivered",
    );
}

/// Return pressed again is what delivers a write whose first Return was
/// swallowed, and a delivery it rescues is an ordinary delivery.
#[tokio::test(start_paused = true)]
async fn pressing_return_again_delivers_a_write_whose_return_was_swallowed() {
    let (server_state, _dir) = test_server_state();
    let (run_id, runner) = parked_codex_worker(&server_state);
    let probe_id = server_state.queue_probe(run_id.clone(), "push your branch".into(), false);
    dispatch_probe_now(&server_state, &run_id).await;

    tokio::time::sleep(TURN_START_CONFIRM_TIMEOUT + Duration::from_secs(1)).await;
    assert_eq!(runner.return_presses(), 2);
    dispatch_live_worker_state(&server_state, &turn_started(&run_id)).await;

    tokio::time::sleep(one_failed_delivery()).await;
    assert_eq!(
        server_state.probe_lifecycle_state(&probe_id),
        Some(ProbeDeliveryState::Consumed),
    );
    assert_eq!(runner.return_presses(), 2, "a delivered probe is not retried");
    assert!(undelivered_probe_attentions(&server_state, &run_id).is_empty());
}

/// A pane someone scrolled is in copy-mode, where tmux consumes the keys and
/// reports success for each. The mode is left before anything is typed.
#[tokio::test(start_paused = true)]
async fn a_pane_left_in_a_tmux_mode_is_taken_out_of_it_before_the_write() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_idle_worker_with_driver(&server_state, SLOT, Some("codex"));
    server_state
        .worker_registry
        .register_tmux_run_slot(&run_id, SLOT, "boss-6");
    register_tmux_identity_for_test(&server_state, &run_id, "boss-6", super::tmux_stub::TEST_SPAWN_TOKEN);
    let runner = std::sync::Arc::new(super::tmux_stub::RecordingPaneRunner::in_copy_mode("boss-6"));
    *server_state.pane_delivery_tmux_override.write().unwrap() =
        Some(super::tmux_stub::tmux_with_runner(runner.clone()));
    server_state.queue_probe(run_id.clone(), "push your branch".into(), false);

    dispatch_probe_now(&server_state, &run_id).await;

    let calls = runner.calls();
    let left_mode = calls
        .iter()
        .position(|call| call.iter().any(|arg| arg == "copy-mode") && call.iter().any(|arg| arg == "-q"))
        .expect("the pane must be taken out of its mode");
    let typed = calls
        .iter()
        .position(|call| call.iter().any(|arg| arg == "-l"))
        .expect("the probe must be typed");
    assert!(
        left_mode < typed,
        "the mode must be left before the text is typed: {calls:?}"
    );
}

/// The window in which a turn boundary is the engine's own interrupt taking
/// effect: an interrupting delivery has claimed its probe and not yet written
/// it. The event fan-out withholds the nudge/park decision for a boundary
/// that lands inside it, because the worker did not stop and is about to be
/// handed the probe. Nothing else may look like that window.
#[tokio::test(start_paused = true)]
async fn only_an_interrupting_delivery_ahead_of_its_write_marks_the_engines_own_boundary() {
    let (server_state, _dir) = test_server_state();
    let run_id = register_working_worker_with_driver(&server_state, SLOT, Some("codex"));
    install_live_tmux_delivery(&server_state, &run_id, SLOT, "boss-6");
    assert!(
        !server_state.probe_claimed_ahead_of_its_write(&run_id),
        "no delivery in progress: a boundary here is the worker stopping",
    );

    let probe_id = server_state.queue_probe(run_id.clone(), "the description changed".into(), false);
    assert!(
        !server_state.probe_claimed_ahead_of_its_write(&run_id),
        "a queued probe nobody has claimed is delivered by the boundary, not ahead of it",
    );

    // What the interrupting path does before it sends Escape.
    server_state
        .try_reserve_specific_probe_for_delivery(&run_id, &probe_id, None, 0)
        .expect("the interrupting delivery claims its probe first");
    assert!(server_state.probe_claimed_ahead_of_its_write(&run_id));

    // Once the write is issued the boundary that follows is the worker's own.
    server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Injected);
    assert!(!server_state.probe_claimed_ahead_of_its_write(&run_id));
}

/// A nudge the ladder has given up on is taken out of the queue, so it cannot
/// arrive late on top of its replacement. One that has already been written
/// is left to its own delivery to settle.
#[tokio::test(start_paused = true)]
async fn a_queued_nudge_can_be_withdrawn_and_a_written_one_cannot() {
    let (server_state, _dir) = test_server_state();
    let (run_id, runner) = parked_codex_worker(&server_state);
    let queuer = ServerStateProbeQueuer::default();
    queuer.set_server_state(Arc::downgrade(&server_state));

    queuer.queue_probe(&run_id, "push your branch");
    assert!(queuer.withdraw_nudge(&run_id, "could not be written into the pane"));
    assert_eq!(server_state.pending_probe_count(&run_id), 0);
    assert!(matches!(
        queuer.nudge_delivery(&run_id),
        NudgeDelivery::Withdrawn { .. }
    ));
    assert!(!queuer.withdraw_nudge(&run_id, "again"), "nothing is left to withdraw");
    assert!(!runner.wrote_text());

    queuer.queue_probe(&run_id, "push your branch");
    dispatch_probe_now(&server_state, &run_id).await;
    assert!(runner.wrote_text());
    assert!(
        !queuer.withdraw_nudge(&run_id, "could not be written into the pane"),
        "a written nudge is settled by its own delivery",
    );
    assert!(matches!(queuer.nudge_delivery(&run_id), NudgeDelivery::Pending { .. }));
}

/// The incident end to end, with the real probe machinery under the ladder:
/// a parked worker whose pane takes writes and starts no turn is never failed
/// as unresponsive, however many sweeps pass, and is in the end failed for
/// what actually went wrong.
#[tokio::test(start_paused = true)]
async fn a_parked_worker_is_not_failed_for_nudges_it_was_never_shown() {
    let (server_state, _dir) = test_server_state();
    let (run_id, runner) = parked_codex_worker(&server_state);
    let queuer = Arc::new(ServerStateProbeQueuer::default());
    queuer.set_server_state(Arc::downgrade(&server_state));
    let TestHarness { handler, cube, .. } =
        TestHarness::with_probe_queuer(server_state.work_db.clone(), StubPrDetector::ok(None), queuer.clone());
    let breaker = Arc::new(NudgeBreaker::new());
    let handler = handler.with_nudge_breaker(breaker.clone());

    // The worker stops without a PR. Nudge 1 is issued and written on that
    // boundary, and starts no turn.
    assert!(matches!(handler.on_stop(&run_id).await, StopOutcome::AwaitingInput));
    assert_eq!(
        dispatch_probe_on_stop(&server_state, &stop(&run_id)).await,
        ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Injected),
    );
    assert_eq!(breaker.counts(&run_id), Some((1, 1)));

    // Sweep after sweep finds the worker idle. Before the fix sweeps 2 and 3
    // each queued and counted a nudge behind the first, and sweep 4 failed
    // the execution with "no new commit, PR, or state change".
    for sweep in 0..(ABSOLUTE_MAX_NUDGES + 2) {
        let outcome = handler.recheck_background_nudge(&run_id).await;
        assert!(
            matches!(outcome, Some(StopOutcome::NudgeDebounced)),
            "sweep {sweep}: an undelivered nudge must hold the ladder, got {outcome:?}",
        );
    }
    assert_eq!(breaker.counts(&run_id), Some((1, 1)));
    assert_eq!(
        server_state.pending_probe_count(&run_id),
        0,
        "nothing stacked behind it"
    );
    assert!(server_state.work_db.get_execution(&run_id).unwrap().status.is_live());

    // Each delivery runs out its retries and is settled as failed; the next
    // sweep hands the nudge back and issues another, until the run of failed
    // deliveries reaches the bound.
    let mut outcome = None;
    for failures in 1..=3 {
        tokio::time::sleep(one_failed_delivery()).await;
        assert!(
            matches!(queuer.nudge_delivery(&run_id), NudgeDelivery::Failed { .. }),
            "delivery {failures} must have been settled as failed",
        );
        outcome = handler.recheck_background_nudge(&run_id).await;
        if failures < 3 {
            assert!(
                matches!(outcome, Some(StopOutcome::AwaitingInput)),
                "failed delivery {failures} is under the bound: re-issue, got {outcome:?}",
            );
            assert_eq!(breaker.delivery_failures(&run_id), failures);
            assert_eq!(
                breaker.counts(&run_id),
                Some((1, 1)),
                "the failed nudges were handed back; only the one outstanding is charged",
            );
            // The sweep's out-of-band delivery writes the replacement.
            tokio::time::sleep(Duration::from_secs(5)).await;
            assert!(matches!(queuer.nudge_delivery(&run_id), NudgeDelivery::Pending { .. }));
        }
    }

    let Some(StopOutcome::NudgeBreakerParked { reason }) = outcome else {
        panic!("a pane that cannot be nudged must still end the attempt, got {outcome:?}");
    };
    assert!(
        reason.contains("auto-nudge delivery failed 3 times in a row") && reason.contains("never started a turn"),
        "the failure must be reported as the delivery failure it is: {reason}",
    );
    assert!(
        !reason.contains("no new commit, PR, or state change"),
        "a worker that was shown nothing was not unresponsive: {reason}",
    );
    assert_eq!(
        runner.return_presses(),
        3 * (1 + RETURN_RETRIES as usize),
        "three nudges, each typed once and retried with Return alone",
    );
    assert_eq!(cube.release_calls.lock().await.len(), 1);
    assert!(!server_state.work_db.get_execution(&run_id).unwrap().status.is_live());
}

// A provisionally consumed interrupting nudge retains its claim until its
// own text is confirmed, even if another interrupting probe arrives.
#[tokio::test(start_paused = true)]
async fn an_unconfirmed_interrupting_nudge_cannot_be_displaced() {
    let (server, _dir) = test_server_state();
    let (run, _) = parked_codex_worker(&server);
    let queuer = ServerStateProbeQueuer::default();
    queuer.set_server_state(Arc::downgrade(&server));
    queuer.queue_probe(&run, "push your branch");
    let first = server.try_reserve_probe_for_delivery(&run, None, 0).unwrap();
    server.set_probe_lifecycle(&first.probe_id, ProbeDeliveryState::Consumed);
    server.set_probe_submission(&first.probe_id, true, Some(ProbeResumeEvidence::Unconfirmed));
    server.mark_in_flight_probe_awaiting_turn_start(&run, &first.probe_id, &first.text);
    let second = server.queue_probe(run.clone(), "another instruction".into(), false);
    assert!(
        server
            .try_reserve_specific_probe_for_delivery(&run, &second, None, 0)
            .is_none()
    );
    assert_eq!(
        server.in_flight_probe_id(&run).as_deref(),
        Some(first.probe_id.as_str())
    );
    assert!(matches!(queuer.nudge_delivery(&run), NudgeDelivery::Pending { .. }));
    dispatch_live_worker_state(&server, &turn_started(&run)).await;
    assert_eq!(
        server.probe_record(&first.probe_id).unwrap().resumed,
        Some(ProbeResumeEvidence::Confirmed)
    );
    server.set_probe_submission(&first.probe_id, true, Some(ProbeResumeEvidence::Unconfirmed));
    assert_eq!(
        server.probe_record(&first.probe_id).unwrap().resumed,
        Some(ProbeResumeEvidence::Confirmed),
        "late write bookkeeping cannot undo submission evidence"
    );
    assert!(matches!(queuer.nudge_delivery(&run), NudgeDelivery::Settled));
    assert!(
        server
            .try_reserve_specific_probe_for_delivery(&run, &second, None, 0)
            .is_some()
    );
}

#[tokio::test(start_paused = true)]
async fn an_unrelated_turn_does_not_confirm_a_parked_probe() {
    let (server, _dir) = test_server_state();
    let (run, _) = parked_codex_worker(&server);
    let probe = server.queue_probe(run.clone(), "push your branch".into(), false);
    dispatch_probe_now(&server, &run).await;
    let unrelated = hook(
        WorkerEvent::UserPromptSubmit {
            session_id: "sess".into(),
            prompt: "what time is it?".into(),
        },
        &run,
    );
    dispatch_live_worker_state(&server, &unrelated).await;
    assert_eq!(server.probe_lifecycle_state(&probe), Some(ProbeDeliveryState::Injected));
    assert!(server.probe_awaiting_turn_start(&run, &probe));
    dispatch_probe_reply_on_stop(&server, &stop(&run)).await;
    assert!(
        server.probe_awaiting_turn_start(&run, &probe),
        "an unrelated Stop cannot take the claim"
    );
    // A later prompt carrying the probe itself proves submission.
    dispatch_live_worker_state(&server, &turn_started(&run)).await;
    assert_eq!(
        server.probe_record(&probe).unwrap().resumed,
        Some(ProbeResumeEvidence::Confirmed)
    );
}

async fn confirmation_during_diagnostics(deadline: u32) {
    let (server, _dir) = test_server_state();
    let (run, _) = parked_codex_worker(&server);
    let pause = Arc::new(super::tmux_stub::ProbePause::default());
    let runner = Arc::new(super::tmux_stub::RecordingPaneRunner::new("boss-6").pause_capture(pause.clone()));
    *server.pane_delivery_tmux_override.write().unwrap() = Some(super::tmux_stub::tmux_with_runner(runner.clone()));
    let probe = server.queue_probe(run.clone(), "push your branch".into(), false);
    server.try_reserve_probe_for_delivery(&run, None, 0).unwrap();
    server.set_probe_lifecycle(&probe, ProbeDeliveryState::Consumed);
    server.set_probe_submission(&probe, true, Some(ProbeResumeEvidence::Unconfirmed));
    server.mark_in_flight_probe_awaiting_turn_start(&run, &probe, "push your branch");
    super::super::parked_delivery::watch_parked_write(&server, &run, SLOT, &probe, "push your branch".into());
    for window in 0..=deadline {
        pause.entered.notified().await;
        if window == deadline {
            dispatch_live_worker_state(&server, &turn_started(&run)).await;
        }
        pause.resume.notify_one();
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(one_failed_delivery()).await;
    let record = server.probe_record(&probe).unwrap();
    assert_eq!(record.state, ProbeDeliveryState::Consumed);
    assert_eq!(record.resumed, Some(ProbeResumeEvidence::Confirmed));
    assert_eq!(
        runner.return_presses(),
        deadline as usize,
        "no Return after confirmation"
    );
    assert!(undelivered_probe_attentions(&server, &run).is_empty());
}

#[tokio::test(start_paused = true)]
async fn confirmation_during_retry_diagnostics_prevents_return() {
    confirmation_during_diagnostics(0).await;
}

#[tokio::test(start_paused = true)]
async fn confirmation_during_final_diagnostics_prevents_failure() {
    confirmation_during_diagnostics(RETURN_RETRIES).await;
}

#[tokio::test(start_paused = true)]
async fn mid_turn_dispatch_that_finds_a_parked_pane_watches_the_echo() {
    let (server, _dir) = test_server_state();
    let run = register_idle_worker_with_driver(&server, SLOT, Some("claude"));
    server.worker_registry.register_tmux_run_slot(&run, SLOT, "boss-6");
    register_tmux_identity_for_test(&server, &run, "boss-6", super::tmux_stub::TEST_SPAWN_TOKEN);
    let runner =
        Arc::new(super::tmux_stub::RecordingPaneRunner::new("boss-6").capture_text("[nudge] push your branch"));
    *server.pane_delivery_tmux_override.write().unwrap() = Some(super::tmux_stub::tmux_with_runner(runner.clone()));
    let probe = server.queue_probe(run.clone(), "push your branch".into(), false);
    let event = hook(
        WorkerEvent::PostToolUse {
            session_id: "sess".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
        &run,
    );
    let outcome = dispatch_probe_on_post_tool_use(&server, &event).await;
    assert_eq!(outcome, ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Injected));
    assert!(server.probe_awaiting_turn_start(&run, &probe));
    tokio::time::sleep(one_failed_delivery()).await;
    assert_eq!(
        server.probe_lifecycle_state(&probe),
        Some(ProbeDeliveryState::Unconfirmed)
    );
    assert!(!server.has_in_flight_probe(&run));
    assert_eq!(runner.return_presses(), 1 + RETURN_RETRIES as usize);
    assert_eq!(undelivered_probe_attentions(&server, &run).len(), 1);
}

struct CompletionDetector(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl boss_engine_completion::PrDetector for CompletionDetector {
    async fn detect_pr(&self, _remote: &str, _branch: &str) -> anyhow::Result<boss_engine_completion::PrStatus> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(boss_engine_completion::PrStatus::None)
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_pre_pass_write_rechecks_the_deferred_stop_decision() {
    let detector = Arc::new(CompletionDetector(std::sync::atomic::AtomicUsize::new(0)));
    let (server, _dir) = test_server_state_with_fakes_and_pr_detector(detector.clone(), None);
    let (run, _) = parked_codex_worker(&server);
    let probe = server.queue_probe(run.clone(), "push your branch".into(), false);
    let outcome = dispatch_probe_on_stop(&server, &stop(&run)).await;
    assert_eq!(outcome, ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Injected));
    dispatch_completion_on_stop(&server, &stop(&run), true).await;
    assert!(server.parked_probe_snapshot(&run).unwrap().deferred_completion);
    assert_eq!(detector.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    tokio::time::sleep(one_failed_delivery()).await;
    assert_eq!(
        server.probe_lifecycle_state(&probe),
        Some(ProbeDeliveryState::Unconfirmed)
    );
    assert!(
        detector.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the failed write must re-drive the PR detection withheld at Stop"
    );
}
