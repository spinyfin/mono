//! Credential proof is scoped to accepted delivery, purpose, and generation.
use super::*;

pub(super) async fn verify(state: &Arc<ServerState>, id: &str, authorization: bool) -> bool {
    let sink = make_session_sink();
    coordinator_handoff::handle_verify_coordinator_reset_request(
        dispatch(state, &sink, "verify-purpose"),
        FrontendRequest::VerifyCoordinatorResetRequest {
            request_id: id.into(),
            authorization,
        },
    )
    .await;
    match next_event(&sink).await.1 {
        FrontendEvent::CoordinatorResetRequestStatus { pending } => pending,
        other => panic!("unexpected reply: {other:?}"),
    }
}

#[tokio::test]
async fn authorization_proof_rejects_reset_purpose_and_replaced_generation() {
    let f = fixture(Duration::from_secs(1)).await;
    assert!(
        coordinator_handoff::deliver_authorization(f.state.clone(), "test")
            .await
            .requested
    );
    let prompts = f.runner.inbox_prompts("[Boss handoff authorization");
    let id = authorization_id_from_prompt(&prompts[0]);
    wait_for_confirmation(&f).await;
    assert!(verify(&f.state, id, true).await);
    assert!(!verify(&f.state, id, false).await);
    f.state
        .work_db
        .record_coordinator_tmux_spawn_intent("boss-coordinator", "replacement", "opus", None)
        .unwrap();
    assert!(!verify(&f.state, id, true).await);
    let previous_turn = f.runner.faults.lock().unwrap().authorization_turn_end.clone();
    f.runner.faults.lock().unwrap().authorization_turn_end = Arc::new(tokio::sync::Notify::new());
    assert!(
        coordinator_handoff::deliver_authorization(f.state.clone(), "test")
            .await
            .requested
    );
    wait_for_confirmation(&f).await;
    let prompts = f.runner.inbox_prompts("[Boss handoff authorization");
    let replacement_id = authorization_id_from_prompt(&prompts[1]);
    previous_turn.notify_one();
    tokio::task::yield_now().await;
    assert!(!verify(&f.state, id, true).await);
    assert!(verify(&f.state, replacement_id, true).await);
}

#[tokio::test]
async fn failed_delivery_never_registers_authorization_proof() {
    let f = fixture(Duration::from_secs(1)).await;
    f.runner.faults.lock().unwrap().fail = Some("inbox unavailable".into());
    assert!(
        !coordinator_handoff::deliver_authorization(f.state.clone(), "test")
            .await
            .requested
    );
    let calls = f.runner.calls();
    let failed = calls
        .iter()
        .find(|c| c.first().map(String::as_str) == Some("inbox") && c[2] == "failed")
        .unwrap();
    let id = authorization_id_from_prompt(&failed[1]);
    assert!(!verify(&f.state, id, true).await);
    assert!(!verify(&f.state, id, false).await);
}

#[tokio::test(start_paused = true)]
async fn proof_survives_input_confirmation_until_turn_end() {
    let f = fixture(Duration::from_secs(1)).await;
    assert!(
        coordinator_handoff::deliver_authorization(f.state.clone(), "test")
            .await
            .requested
    );
    let prompts = f.runner.inbox_prompts("[Boss handoff authorization");
    let id = authorization_id_from_prompt(&prompts[0]);
    wait_for_confirmation(&f).await;
    tokio::time::advance(coordinator_handoff::PENDING_COALESCE_WINDOW - Duration::from_secs(1)).await;
    assert!(verify(&f.state, id, true).await);
    assert!(verify(&f.state, id, true).await, "verification is idempotent");
    f.runner.faults.lock().unwrap().authorization_turn_end.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while f
            .state
            .coordinator_authorization_delivery_pending
            .lock()
            .unwrap()
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!verify(&f.state, id, true).await);
}

#[tokio::test(start_paused = true)]
async fn confirmation_timeout_withdraws_authorization_proof() {
    let f = fixture(Duration::from_secs(1)).await;
    f.runner.faults.lock().unwrap().hold_confirmation = true;
    assert!(
        coordinator_handoff::deliver_authorization(f.state.clone(), "test")
            .await
            .requested
    );
    let prompts = f.runner.inbox_prompts("[Boss handoff authorization");
    let id = authorization_id_from_prompt(&prompts[0]);
    assert!(verify(&f.state, id, true).await);
    tokio::task::yield_now().await;
    tokio::time::advance(
        boss_engine_runtime::coordinator_tmux::inbox::AUTHORIZATION_CONFIRMATION_TIMEOUT + Duration::from_secs(1),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while f
            .state
            .coordinator_authorization_delivery_pending
            .lock()
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!verify(&f.state, id, true).await);
}

#[tokio::test]
async fn confirmation_failure_withdraws_authorization_proof() {
    let f = fixture(Duration::from_secs(1)).await;
    f.runner.faults.lock().unwrap().confirmation_failure = true;
    assert!(
        coordinator_handoff::deliver_authorization(f.state.clone(), "test")
            .await
            .requested
    );
    let prompts = f.runner.inbox_prompts("[Boss handoff authorization");
    let id = authorization_id_from_prompt(&prompts[0]);
    tokio::time::timeout(Duration::from_secs(2), async {
        while f
            .state
            .coordinator_authorization_delivery_pending
            .lock()
            .unwrap()
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!verify(&f.state, id, true).await);
}

/// A confirmed delivery whose input turn never ends (compaction, `/clear`,
/// or a transcript read error) must not suppress `--redeliver` forever.
/// After [`coordinator_handoff::PENDING_COALESCE_WINDOW`], a later request
/// sends a fresh id; the previous one no longer verifies.
#[tokio::test(start_paused = true)]
async fn missed_turn_end_releases_authorization_after_coalesce_window() {
    let f = fixture(Duration::from_secs(1)).await;
    assert!(
        coordinator_handoff::deliver_authorization(f.state.clone(), "test")
            .await
            .requested
    );
    let prompts = f.runner.inbox_prompts("[Boss handoff authorization");
    let id = authorization_id_from_prompt(&prompts[0]).to_owned();
    wait_for_confirmation(&f).await;
    assert!(verify(&f.state, &id, true).await);
    let coalesced = coordinator_handoff::deliver_authorization(f.state.clone(), "test").await;
    assert!(
        coalesced.requested && coalesced.detail.contains("already pending"),
        "{coalesced:?}"
    );
    assert_eq!(f.runner.inbox_prompts("[Boss handoff authorization").len(), 1);
    tokio::time::advance(coordinator_handoff::PENDING_COALESCE_WINDOW + Duration::from_secs(1)).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while f
            .state
            .coordinator_authorization_delivery_pending
            .lock()
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!verify(&f.state, &id, true).await);
    assert!(
        coordinator_handoff::deliver_authorization(f.state.clone(), "test")
            .await
            .requested
    );
    let prompts = f.runner.inbox_prompts("[Boss handoff authorization");
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    let new_id = authorization_id_from_prompt(&prompts[1]);
    assert_ne!(new_id, id);
    assert!(verify(&f.state, new_id, true).await);
    assert!(!verify(&f.state, &id, true).await);
}

async fn wait_for_confirmation(f: &Fixture) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !f
            .state
            .work_db
            .coordinator_handoff_authorization_delivered_to(
                &f.state.work_db.coordinator_tmux_record().unwrap().unwrap().spawn_token,
            )
            .unwrap()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
