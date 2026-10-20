use super::*;

#[tokio::test(start_paused = true)]
async fn old_session_retry_preserves_new_session_scheduled_flag() {
    let (state, _dir) = super::super::tests::test_server_state();
    state.spawn_worker_viewer_reattach_retry("old-session".into());
    tokio::task::yield_now().await;
    state.viewer_reattach_epoch.fetch_add(1, Ordering::Relaxed);
    state.viewer_reattach_retry_attempt.store(1, Ordering::Relaxed);
    state.viewer_reattach_retry_scheduled.store(true, Ordering::Relaxed);
    tokio::time::advance(VIEWER_REATTACH_RETRY_BASE).await;
    tokio::task::yield_now().await;
    assert!(state.viewer_reattach_retry_scheduled.load(Ordering::Relaxed));
    assert_eq!(state.viewer_reattach_retry_attempt.load(Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn fresh_spawn_failure_restarts_exhausted_retry_budget() {
    use crate::spawn_flow::WorkerSpawner;
    let (state, _dir) = super::super::tests::test_server_state();
    let sink = super::super::tests::make_session_sink();
    state.register_app_session("app".into(), sink).await;
    state
        .viewer_reattach_retry_attempt
        .store(VIEWER_REATTACH_RETRY_MAX_ATTEMPTS, Ordering::Relaxed);
    state.note_worker_viewer_attach_failed("new-run").await;
    assert!(state.viewer_reattach_retry_scheduled.load(Ordering::Relaxed));
    assert_eq!(state.viewer_reattach_retry_attempt.load(Ordering::Relaxed), 1);
}
