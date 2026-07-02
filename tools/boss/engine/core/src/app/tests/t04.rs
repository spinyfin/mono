// Coverage for the pane-vacate reliability work added after the
// zombie-Riker/Garak-pane incident (2026-07-01/02): an execution whose
// engine-side slot mapping was already dropped by a prior
// (app-unreachable) teardown attempt left NO verb able to close the
// app's still-rendered pane. `release_worker_pane_detailed` reports
// whether the app actually confirmed the close (not just "the engine's
// own backstop ran"), and `force_vacate_run` uses that signal to fall
// back to a run-id-keyed `VacatePaneByRunId` request the app answers by
// scanning its own pane inventory, independent of any engine-side
// bookkeeping.

use super::*;

fn seed_real_execution(server_state: &ServerState) -> String {
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("c")
                .autostart(false)
                .build(),
        )
        .unwrap();
    server_state
        .work_db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap()
        .id
}

/// Pull the next `EngineRequest` off `sink` and reply with a canned
/// success matching whichever pane-teardown variant it carries.
async fn auto_ack_pane_teardown(server_state: &ServerState, sink: &SessionSink) -> EngineToAppRequest {
    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    let response = match &request {
        EngineToAppRequest::ReleaseWorkerPane(_) => EngineToAppResponse::ReleaseWorkerPane {
            result: Ok(crate::protocol::ReleaseWorkerPaneResult {}),
        },
        EngineToAppRequest::VacatePaneByRunId(_) => EngineToAppResponse::VacatePaneByRunId {
            result: Ok(crate::protocol::VacatePaneByRunIdResult { vacated: true }),
        },
        other => panic!("unexpected EngineRequest in test: {other:?}"),
    };
    server_state
        .deliver_app_response("session-app", &request_id, response)
        .await;
    request
}

#[tokio::test]
async fn release_worker_pane_detailed_reports_app_confirmed_on_ack() {
    let server_state = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;
    server_state.worker_registry.register_run_slot("run-1", 1);

    let server_clone = server_state.clone();
    let call = tokio::spawn(async move { server_clone.release_worker_pane_detailed("run-1").await });
    let acked = auto_ack_pane_teardown(&server_state, &sink).await;
    assert!(matches!(acked, EngineToAppRequest::ReleaseWorkerPane(_)));

    let report = call.await.expect("task panicked");
    assert_eq!(report.outcome, PaneReleaseOutcome::Reaped);
    assert!(
        report.app_confirmed,
        "app positively acknowledged the release; app_confirmed must be true",
    );
}

#[tokio::test]
async fn release_worker_pane_detailed_reports_unconfirmed_when_no_app_session() {
    // This is the exact gap behind the incident: the engine-side
    // backstop (process signal, pool-slot release, live-state drop)
    // still runs and reports `Reaped`, but nobody ever told the app to
    // close the pane — `app_confirmed` must say so.
    let server_state = test_server_state();
    server_state.worker_registry.register_run_slot("run-1", 1);

    let report = server_state.release_worker_pane_detailed("run-1").await;
    assert_eq!(report.outcome, PaneReleaseOutcome::Reaped);
    assert!(
        !report.app_confirmed,
        "no app session was registered; app_confirmed must be false even though the engine backstop ran",
    );
}

#[tokio::test]
async fn force_vacate_run_skips_app_round_trip_for_nonexistent_execution() {
    // No slot mapped and no execution row at all: there both never was
    // and never could be a pane. This must resolve immediately without
    // attempting (and timing out on) an app round trip — proven by
    // wrapping the call in a short deadline.
    let server_state = test_server_state();
    let outcome = tokio::time::timeout(
        Duration::from_millis(200),
        server_state.force_vacate_run("run-does-not-exist"),
    )
    .await
    .expect("must resolve without an app round trip");
    assert!(matches!(outcome, ForceVacateOutcome::NothingToVacate));
}

#[tokio::test]
async fn force_vacate_run_falls_back_to_run_id_keyed_vacate_when_slot_mapping_gone() {
    // The corpse-pane recovery scenario itself: a real execution row
    // exists, but no slot mapping does (as if a prior teardown attempt
    // already consumed it via `take_slot_for_run` while the app was
    // unreachable). With an app session now attached, `force_vacate_run`
    // must fall through to the run-id-keyed `VacatePaneByRunId` request.
    let server_state = test_server_state();
    let execution_id = seed_real_execution(&server_state);
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let execution_id_for_task = execution_id.clone();
    let call = tokio::spawn(async move { server_clone.force_vacate_run(&execution_id_for_task).await });
    let acked = auto_ack_pane_teardown(&server_state, &sink).await;
    assert!(
        matches!(acked, EngineToAppRequest::VacatePaneByRunId(_)),
        "expected the run-id-keyed fallback, got {acked:?}",
    );

    let outcome = call.await.expect("task panicked");
    assert!(matches!(outcome, ForceVacateOutcome::Vacated));
}

#[tokio::test]
async fn force_vacate_run_returns_app_unreachable_for_existing_execution_with_no_app() {
    // The "fail loudly" half of the contract: a real execution with no
    // app session at all must not be reported as vacated.
    let server_state = test_server_state();
    let execution_id = seed_real_execution(&server_state);

    let outcome = server_state.force_vacate_run(&execution_id).await;
    assert!(
        matches!(outcome, ForceVacateOutcome::AppUnreachable(_)),
        "expected AppUnreachable, got {outcome:?}",
    );
}

#[tokio::test]
async fn force_vacate_run_reports_vacated_from_slot_keyed_tier_without_fallback() {
    // The common case — a live slot mapping and a responsive app —
    // must resolve via the normal `ReleaseWorkerPane` round trip alone;
    // the run-id-keyed fallback must not be needed (and therefore never
    // requested) when the first tier already confirmed.
    let server_state = test_server_state();
    server_state.worker_registry.register_run_slot("run-1", 1);
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let call = tokio::spawn(async move { server_clone.force_vacate_run("run-1").await });
    let acked = auto_ack_pane_teardown(&server_state, &sink).await;
    assert!(matches!(acked, EngineToAppRequest::ReleaseWorkerPane(_)));

    let outcome = call.await.expect("task panicked");
    assert!(matches!(outcome, ForceVacateOutcome::Vacated));

    // No second EngineRequest should be queued — draining with a short
    // deadline must find nothing.
    let peek = tokio::time::timeout(Duration::from_millis(100), sink.next()).await;
    assert!(
        peek.is_err(),
        "the fallback VacatePaneByRunId must not fire once the slot-keyed tier confirmed",
    );
}
