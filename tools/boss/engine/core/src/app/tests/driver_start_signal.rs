//! Ingress-side driver-start verification.
//!
//! `spawn_ack_sweep`'s driver-start pass reaps any live slot that has gone
//! its grace window without a driver-originated signal. That is only safe
//! because the hook ingress records the signal for every worker whose
//! driver really is running — so these tests guard the producer side of
//! that contract. If the ingress stopped recording, nothing else in the
//! suite would fail, and every healthy worker would become reapable.

use super::*;

use crate::test_support::*;

/// A real hook arriving at the ingress must stamp the run's driver-start
/// signal, so `spawn_ack_sweep`'s driver-start pass never reaps a worker
/// whose driver is demonstrably running.
///
/// This is the production half of the contract that
/// `live_worker_state::tests::apply_event_alone_does_not_stamp_the_driver_signal`
/// pins from the other side: `apply_event` deliberately does not stamp it,
/// so if the ingress ever stopped calling `record_driver_signal` the signal
/// would silently never be recorded and every healthy worker would become
/// reapable after the grace window. Nothing else would fail.
#[tokio::test]
async fn a_hook_at_the_ingress_records_the_driver_start_signal() {
    use crate::protocol::WorkerEvent;
    use boss_protocol::RequestExecutionInput;

    let (server_state, _dir) = test_server_state();
    let product = create_test_product_with_repo(&server_state.work_db, "p", Some("git@example.com:p.git"));
    let chore = create_test_chore_manual(&server_state.work_db, product.id.clone(), "c");
    let execution = server_state
        .work_db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();

    // A pane that reported a live shell pid but has produced no driver
    // signal — the 2026-07-30 shape, and the state a healthy worker also
    // passes through in its first moments.
    server_state
        .live_worker_states
        .register_spawn(1, execution.id.clone(), "claude-opus-4-7", 4242, None);
    server_state.worker_registry.register_run_slot(&execution.id, 1);
    assert!(
        server_state.live_worker_states.driver_signal_at(1).is_none(),
        "precondition: no driver signal before the first hook",
    );

    let event = crate::events_socket::IncomingHookEvent::for_test(
        WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        },
        Some(execution.id.clone()),
        None,
    );
    dispatch_live_worker_state(&server_state, &event).await;

    assert!(
        server_state.live_worker_states.driver_signal_at(1).is_some(),
        "a hook is driver-originated proof and must be recorded as such",
    );
    assert!(
        server_state
            .live_worker_states
            .unverified_driver_starts(
                boss_engine_utils::epoch_time::now_epoch_secs() + crate::live_worker_state::DRIVER_START_GRACE_SECS * 2,
                crate::live_worker_state::DRIVER_START_GRACE_SECS,
            )
            .is_empty(),
        "having signalled, the slot must never appear as a never-started driver",
    );
}
