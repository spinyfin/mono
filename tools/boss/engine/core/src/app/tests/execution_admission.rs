//! Admission evaluation and pause-only force-start behaviour.

use super::*;
use crate::app::engine_meta;
use crate::app::executions;
use crate::coordinator::DispatchPauseOrigin;
use crate::execution_admission::{self, AdmissionIntent, AdmissionRuntimeSnapshot};
use boss_protocol::{
    CreateChoreInput, ExecutionRequestEntryPoint, ExecutionStatus, FrontendRequest, PauseReason, RequestExecutionInput,
    TaskStatus,
};

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

async fn pause_dispatch(state: &Arc<ServerState>, reason: &str) {
    let sink = make_session_sink();
    let ctx = dispatch(state, &sink);
    engine_meta::handle_set_dispatch_paused(
        ctx,
        FrontendRequest::SetDispatchPaused {
            paused: true,
            reason: Some(reason.to_owned()),
        },
    )
    .await;
    let _ = sole_response(&sink).await;
}

fn make_chore(state: &Arc<ServerState>, name: &str) -> String {
    let product = crate::test_support::create_test_product(&state.work_db);
    let chore = state
        .work_db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id)
                .name(name)
                .autostart(false)
                .build(),
        )
        .unwrap();
    chore.id
}

#[tokio::test]
async fn evaluate_reports_operator_pause_as_overridable() {
    let (server_state, _dir) = test_server_state();
    let chore_id = make_chore(&server_state, "Eval pause");
    pause_dispatch(&server_state, "investigating worker failures").await;

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink);
    executions::handle_evaluate_execution_admission(
        ctx,
        FrontendRequest::EvaluateExecutionAdmission {
            work_item_id: chore_id.clone(),
            bypass_dispatch_pause: false,
            observed_pause_generation: None,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ExecutionAdmissionResult { evaluation } => {
            assert!(!evaluation.would_admit);
            assert!(evaluation.pause_overridable);
            assert!(
                evaluation
                    .blockers
                    .iter()
                    .any(|b| b.code == "dispatch_paused" && b.force_overridable),
                "expected overridable dispatch_paused blocker, got {:?}",
                evaluation.blockers
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[tokio::test]
async fn force_start_intent_admits_through_operator_pause() {
    let (server_state, _dir) = test_server_state();
    let chore_id = make_chore(&server_state, "Force while paused");
    pause_dispatch(&server_state, "holding the queue").await;
    assert!(server_state.execution_coordinator.is_dispatch_paused());

    let runtime = AdmissionRuntimeSnapshot::from_coordinator(&server_state.execution_coordinator).await;
    let evaluation = execution_admission::evaluate_execution_admission(
        &server_state.work_db,
        &chore_id,
        AdmissionIntent {
            bypass_dispatch_pause: true,
            observed_pause_generation: None,
        },
        &runtime,
    )
    .unwrap();
    assert!(
        evaluation.would_admit,
        "force should admit through operator pause: {:?}",
        evaluation.blockers
    );
    assert!(evaluation.would_override_pause);
    assert!(
        server_state.execution_coordinator.is_dispatch_paused(),
        "evaluation must not resume global dispatch"
    );
}

#[tokio::test]
async fn evaluate_with_bypass_omits_pause_as_hard_blocker() {
    let (server_state, _dir) = test_server_state();
    let chore_id = make_chore(&server_state, "Bypass preview");
    pause_dispatch(&server_state, "holding").await;

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink);
    executions::handle_evaluate_execution_admission(
        ctx,
        FrontendRequest::EvaluateExecutionAdmission {
            work_item_id: chore_id,
            bypass_dispatch_pause: true,
            observed_pause_generation: None,
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::ExecutionAdmissionResult { evaluation } => {
            assert!(evaluation.would_admit, "{:?}", evaluation.blockers);
            assert!(evaluation.would_override_pause);
            assert!(
                !evaluation
                    .blockers
                    .iter()
                    .any(|b| b.code == "dispatch_paused" && !b.force_overridable),
                "bypass preview must not hard-block on operator pause"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn force_start_dependency_blocked_leaves_no_ready_residue() {
    let (server_state, _dir) = test_server_state();
    let product = crate::test_support::create_test_product(&server_state.work_db);
    let prereq = server_state
        .work_db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Prereq")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let gated = server_state
        .work_db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id)
                .name("Gated")
                .autostart(false)
                .build(),
        )
        .unwrap();
    server_state
        .work_db
        .add_dependency(boss_protocol::AddDependencyInput {
            dependent: gated.id.clone(),
            prerequisite: prereq.id.clone(),
            relation: None,
        })
        .unwrap();
    pause_dispatch(&server_state, "paused").await;

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink);
    executions::handle_request_execution(
        ctx,
        FrontendRequest::RequestExecution {
            input: RequestExecutionInput {
                work_item_id: gated.id.clone(),
                bypass_dispatch_pause: true,
                entry_point: Some(ExecutionRequestEntryPoint::Cli),
                ..Default::default()
            },
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("gated") || message.contains("depend"),
                "expected dependency refusal, got {message}"
            );
        }
        other => panic!("expected WorkError, got {other:?}"),
    }
    let execs = server_state.work_db.list_executions(Some(&gated.id)).unwrap();
    assert!(
        execs
            .iter()
            .all(|e| e.status.is_terminal() || e.status != ExecutionStatus::Ready),
        "refusal must leave no ready residue: {execs:?}"
    );
    // Actually: no executions at all is ideal.
    assert!(
        execs.is_empty() || execs.iter().all(|e| e.status.is_terminal()),
        "no newly queued ready execution; got {execs:?}"
    );
}

#[tokio::test]
async fn stale_pause_generation_is_refused() {
    let (server_state, _dir) = test_server_state();
    let chore_id = make_chore(&server_state, "Stale gen");
    pause_dispatch(&server_state, "first reason").await;
    let first_generation = server_state
        .execution_coordinator
        .dispatch_paused_since_epoch_s()
        .expect("paused");

    // Re-pause with a new generation (resume then pause).
    {
        let sink = make_session_sink();
        let ctx = dispatch(&server_state, &sink);
        engine_meta::handle_set_dispatch_paused(
            ctx,
            FrontendRequest::SetDispatchPaused {
                paused: false,
                reason: None,
            },
        )
        .await;
        let _ = sole_response(&sink).await;
    }
    // Ensure a different generation by sleeping past the same-second edge.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    pause_dispatch(&server_state, "second reason").await;
    let second_generation = server_state
        .execution_coordinator
        .dispatch_paused_since_epoch_s()
        .expect("paused again");
    assert_ne!(first_generation, second_generation, "test needs distinct generations");

    let sink = make_session_sink();
    let ctx = dispatch(&server_state, &sink);
    executions::handle_request_execution(
        ctx,
        FrontendRequest::RequestExecution {
            input: RequestExecutionInput {
                work_item_id: chore_id,
                bypass_dispatch_pause: true,
                entry_point: Some(ExecutionRequestEntryPoint::AppDrag),
                observed_pause_generation: Some(first_generation),
                ..Default::default()
            },
        },
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("changed") || message.contains("confirm"),
                "expected stale confirmation refusal, got {message}"
            );
        }
        other => panic!("expected WorkError for stale generation, got {other:?}"),
    }
}

#[tokio::test]
async fn breaker_pause_is_not_overridable() {
    let (server_state, _dir) = test_server_state();
    let chore_id = make_chore(&server_state, "Breaker");
    let now = boss_engine_utils::epoch_time::now_epoch_secs() as u64;
    server_state.execution_coordinator.pause_dispatch(
        now,
        DispatchPauseOrigin::Breaker,
        PauseReason::new("spawn path unhealthy").unwrap(),
    );

    let runtime = AdmissionRuntimeSnapshot::from_coordinator(&server_state.execution_coordinator).await;
    let evaluation = execution_admission::evaluate_execution_admission(
        &server_state.work_db,
        &chore_id,
        AdmissionIntent {
            bypass_dispatch_pause: true,
            observed_pause_generation: None,
        },
        &runtime,
    )
    .unwrap();
    assert!(!evaluation.would_admit);
    assert!(!evaluation.pause_overridable);
    assert!(
        evaluation
            .blockers
            .iter()
            .any(|b| b.code == "dispatch_paused" && !b.force_overridable),
        "breaker pause must not be force_overridable: {:?}",
        evaluation.blockers
    );
}

#[tokio::test]
async fn force_does_not_use_pool_growth_bit() {
    // Agents launch still uses force=true; work start force uses
    // bypass_dispatch_pause. Ensure the two stay distinct on the wire shape
    // we construct here (compile-time + runtime field check).
    let input = RequestExecutionInput {
        work_item_id: "task_x".into(),
        force: false,
        bypass_dispatch_pause: true,
        entry_point: Some(ExecutionRequestEntryPoint::Cli),
        ..Default::default()
    };
    assert!(!input.force);
    assert!(input.bypass_dispatch_pause);
}

#[test]
fn ordinary_and_forced_differ_only_on_pause() {
    // Pure evaluator: without pause, bypass does not change would_admit.
    let (server_state, _dir) = test_server_state();
    let chore_id = make_chore(&server_state, "No pause");
    let runtime = AdmissionRuntimeSnapshot {
        dispatch_paused: false,
        pause_origin: None,
        pause_reason: None,
        paused_since_epoch_s: None,
        reviews_exempt: false,
        preflight_block_reason: None,
        interactive_busy: 0,
        interactive_cap: 8,
    };
    let plain = execution_admission::evaluate_execution_admission(
        &server_state.work_db,
        &chore_id,
        AdmissionIntent::default(),
        &runtime,
    )
    .unwrap();
    let forced = execution_admission::evaluate_execution_admission(
        &server_state.work_db,
        &chore_id,
        AdmissionIntent {
            bypass_dispatch_pause: true,
            observed_pause_generation: None,
        },
        &runtime,
    )
    .unwrap();
    assert_eq!(plain.would_admit, forced.would_admit);
    assert!(!forced.would_override_pause);
}

// Silence unused TaskStatus import if not needed in some cfgs.
#[allow(dead_code)]
fn _status() -> TaskStatus {
    TaskStatus::Todo
}
