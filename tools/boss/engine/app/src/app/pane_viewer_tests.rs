//! The read-only remote pane viewer through the real subscribe / unsubscribe
//! handlers and the topic broker (hub-and-spoke design §2.8).
use super::{
    Dispatch, ServerStateOverrides, SessionSink, pane_viewers::PaneSnapshot, subscriptions,
    tests::test_server_state_with_overrides,
};
use anyhow::Result;
use async_trait::async_trait;
use boss_execution_contracts::*;
use boss_protocol::{
    FrontendEvent, FrontendRequest, PaneCaptureState, TopicEventPayload, WorkExecution, WorkItem, pane_capture_topic,
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

const RUN: &str = "execution";

struct Remote {
    answer: Mutex<PaneCaptureOutcome>,
    requests: Mutex<Vec<PaneCaptureRequest>>,
}
struct Provider(Arc<Remote>);

#[async_trait]
impl HostAdapterProvider for Provider {
    async fn adapter_for(&self, _: &boss_work_db::Host) -> Result<Arc<dyn HostAdapter>> {
        Ok(self.0.clone())
    }
}

impl Remote {
    fn new(answer: PaneCaptureOutcome) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            requests: Mutex::new(vec![]),
        })
    }
    fn opens(&self) -> usize {
        self.requests.lock().unwrap().iter().filter(|r| !r.cancel).count()
    }
    /// The subscription id of the most recent open or renewal.
    fn latest_subscription(&self) -> String {
        let requests = self.requests.lock().unwrap();
        let open = requests.iter().rev().find(|r| !r.cancel);
        open.expect("an open").subscription_id.clone()
    }
    fn cancels(&self) -> usize {
        self.requests.lock().unwrap().iter().filter(|r| r.cancel).count()
    }
}

#[async_trait]
impl HostAdapter for Remote {
    fn host_id(&self) -> &str {
        "spoke"
    }
    fn command_repr(&self, _: &[&str]) -> Option<(String, String)> {
        None
    }
    async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
        unreachable!()
    }
    async fn lease_workspace(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: bool,
        _: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        unreachable!()
    }
    async fn release_workspace(&self, _: &str) -> Result<()> {
        unreachable!()
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
        unreachable!()
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
        unreachable!()
    }
    async fn create_change(&self, _: &Path, _: &str) -> Result<CubeChangeHandle> {
        unreachable!()
    }
    async fn goto_workspace(&self, _: &Path, _: u64) -> Result<()> {
        unreachable!()
    }
    async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
        unreachable!()
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        unreachable!()
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        unreachable!()
    }
    async fn spawn_worker(
        &self,
        _: &str,
        _: &WorkExecution,
        _: &WorkItem,
        _: &Path,
        _: Option<&str>,
    ) -> Result<RunOutcome> {
        unreachable!()
    }
    async fn capture_worker_pane(&self, _: &str, request: &PaneCaptureRequest) -> Result<PaneCaptureOutcome> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(if request.cancel {
            PaneCaptureOutcome::Accepted
        } else {
            self.answer.lock().unwrap().clone()
        })
    }
}

fn state_with(remote: &Arc<Remote>, spoke_run: bool) -> (Arc<super::ServerState>, tempfile::TempDir) {
    let (state, dir) = test_server_state_with_overrides(ServerStateOverrides {
        host_adapter_provider: Some(Arc::new(Provider(remote.clone()))),
        ..Default::default()
    });
    state.work_db.add_host("spoke", "worker@spoke", 1, &[]).unwrap();
    state
        .work_db
        .set_host_transport("spoke", boss_work_db::HostTransport::SpokeReady)
        .unwrap();
    if spoke_run {
        state.work_db.reserve_spoke_assignment(RUN, "spoke").unwrap();
    }
    state.pane_viewers.set_renew_every(Duration::from_millis(40));
    (state, dir)
}

fn session_sink() -> Arc<SessionSink> {
    let (shutdown, _rx) = tokio::sync::oneshot::channel::<()>();
    Arc::new(SessionSink::new(
        shutdown,
        crate::app::frontend_metrics::test_registry(),
    ))
}

/// A viewer session: registered with the broker, then subscribed through the
/// real handler.
async fn viewer(state: &Arc<super::ServerState>, id: &str) -> Arc<SessionSink> {
    let sink = session_sink();
    state.topic_broker.register_session(id, sink.clone()).await;
    subscriptions::handle_subscribe(
        Dispatch::builder()
            .server_state(state.clone())
            .work_db(state.work_db.clone())
            .sink(sink.clone())
            .session_id(id)
            .request_id("subscribe")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build(),
        FrontendRequest::Subscribe {
            topics: vec![pane_capture_topic(RUN)],
        },
    )
    .await;
    sink
}

async fn unsubscribe(state: &Arc<super::ServerState>, id: &str) {
    let sink = session_sink();
    subscriptions::handle_unsubscribe(
        Dispatch::builder()
            .server_state(state.clone())
            .work_db(state.work_db.clone())
            .sink(sink)
            .session_id(id)
            .request_id("unsubscribe")
            .recv_instant(std::time::Instant::now())
            .decode_ms(0.0)
            .build(),
        FrontendRequest::Unsubscribe {
            topics: vec![pane_capture_topic(RUN)],
        },
    )
    .await;
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// The next pane capture pushed to `sink`, skipping the handler's own reply.
async fn next_capture(sink: &SessionSink) -> (PaneCaptureState, String, String, bool, Option<String>) {
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(10), sink.next())
            .await
            .expect("no pane capture push")
            .expect("sink closed");
        if let FrontendEvent::TopicEvent {
            event:
                TopicEventPayload::WorkerPaneCapture {
                    state,
                    text,
                    host_id,
                    truncated,
                    detail,
                    ..
                },
            ..
        } = envelope.payload
        {
            return (state, text, host_id, truncated, detail);
        }
    }
}

fn snapshot(remote: &Remote, text: &str) -> Arc<PaneSnapshot> {
    Arc::new(
        PaneSnapshot::builder()
            .run_id(RUN)
            .host_id("spoke")
            .subscription_id(remote.latest_subscription())
            .text(text)
            .truncated(true)
            .observed_unix_ms(1_700_000_000_000)
            .build(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_gets_escaped_text_and_leaving_cancels_the_capture() {
    let remote = Remote::new(PaneCaptureOutcome::Accepted);
    let (state, _dir) = state_with(&remote, true);
    let sink = viewer(&state, "viewer-a").await;
    until("the capture to open", || remote.opens() >= 1).await;

    let feed = state.pane_viewers.feed();
    feed.send(snapshot(&remote, "ready \x1b[31mred\x07\nnext")).unwrap();
    let (capture_state, text, host, truncated, _) = next_capture(&sink).await;
    assert_eq!(capture_state, PaneCaptureState::Live);
    assert_eq!(text, "ready \\x1b[31mred\\x07\nnext");
    assert_eq!((host.as_str(), truncated), ("spoke", true));
    // Another run's, or another subscription's, snapshot never reaches it.
    let mut other = (*snapshot(&remote, "elsewhere")).clone();
    other.subscription_id = "someone-else".into();
    feed.send(Arc::new(other)).unwrap();
    feed.send(snapshot(&remote, "again")).unwrap();
    assert_eq!(next_capture(&sink).await.1, "again");

    // The capture is renewed while the viewer stays.
    let opened = remote.opens();
    until("a renewal", || remote.opens() > opened).await;
    assert_eq!(remote.cancels(), 0);

    unsubscribe(&state, "viewer-a").await;
    until("the cancel", || remote.cancels() == 1).await;
    until("the task to end", || {
        futures_executor_block(state.pane_viewers.active_runs()) == 0
    })
    .await;
    let opens = remote.opens();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(remote.opens(), opens, "nothing renews after the viewer left");
    assert_eq!(remote.cancels(), 1);
}

fn futures_executor_block<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disconnected_viewer_stops_the_capture_but_a_second_viewer_keeps_it() {
    let remote = Remote::new(PaneCaptureOutcome::Accepted);
    let (state, _dir) = state_with(&remote, true);
    let _first = viewer(&state, "viewer-a").await;
    let second = viewer(&state, "viewer-b").await;
    until("the capture to open", || remote.opens() >= 1).await;

    // One of two goes away without unsubscribing.
    state.topic_broker.remove_session("viewer-a").await;
    state.pane_viewers.viewers_may_have_left().await;
    let opened = remote.opens();
    until("a renewal after one viewer left", || remote.opens() > opened).await;
    assert_eq!(remote.cancels(), 0, "the remaining viewer still needs it");
    state.pane_viewers.feed().send(snapshot(&remote, "still here")).unwrap();
    assert_eq!(next_capture(&second).await.1, "still here");

    state.topic_broker.remove_session("viewer-b").await;
    state.pane_viewers.viewers_may_have_left().await;
    until("the cancel", || remote.cancels() == 1).await;
    until("the task to end", || {
        futures_executor_block(state.pane_viewers.active_runs()) == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_without_the_capture_capability_is_reported_not_retried_into_silence() {
    let remote = Remote::new(PaneCaptureOutcome::Unsupported(
        "host spoke did not negotiate pane capture".into(),
    ));
    let (state, _dir) = state_with(&remote, true);
    let sink = viewer(&state, "viewer-a").await;
    let (capture_state, text, host, _, detail) = next_capture(&sink).await;
    assert_eq!(capture_state, PaneCaptureState::Unsupported);
    assert_eq!((text.as_str(), host.as_str()), ("", "spoke"));
    assert_eq!(detail.as_deref(), Some("host spoke did not negotiate pane capture"));
    // The same answer to a renewal is not pushed again.
    let opened = remote.opens();
    until("two more renewals", || remote.opens() >= opened + 2).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), next_capture(&sink))
            .await
            .is_err(),
        "an unchanged failure is not re-announced"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_host_is_reported_and_recovers_on_its_own() {
    let remote = Remote::new(PaneCaptureOutcome::Unreachable("host spoke is not connected".into()));
    let (state, _dir) = state_with(&remote, true);
    let sink = viewer(&state, "viewer-a").await;
    assert_eq!(next_capture(&sink).await.0, PaneCaptureState::Unreachable);

    *remote.answer.lock().unwrap() = PaneCaptureOutcome::Accepted;
    let opened = remote.opens();
    until("the next renewal", || remote.opens() > opened).await;
    state.pane_viewers.feed().send(snapshot(&remote, "back")).unwrap();
    let (capture_state, text, ..) = next_capture(&sink).await;
    assert_eq!((capture_state, text.as_str()), (PaneCaptureState::Live, "back"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_the_host_no_longer_has_ends_the_view() {
    let remote = Remote::new(PaneCaptureOutcome::Ended);
    let (state, _dir) = state_with(&remote, true);
    let sink = viewer(&state, "viewer-a").await;
    assert_eq!(next_capture(&sink).await.0, PaneCaptureState::Ended);
    until("the task to end", || {
        futures_executor_block(state.pane_viewers.active_runs()) == 0
    })
    .await;
    assert_eq!(remote.cancels(), 0, "there is nothing left to cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_worker_has_no_remote_view_and_never_calls_a_host() {
    let remote = Remote::new(PaneCaptureOutcome::Accepted);
    let (state, _dir) = state_with(&remote, false);
    let sink = viewer(&state, "viewer-a").await;
    let (capture_state, .., detail) = next_capture(&sink).await;
    assert_eq!(capture_state, PaneCaptureState::Unsupported);
    assert!(detail.unwrap().contains("spoke-connected"));
    assert!(remote.requests.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_departure_that_leaves_viewers_asks_the_host_for_nothing() {
    let remote = Remote::new(PaneCaptureOutcome::Accepted);
    let (state, _dir) = state_with(&remote, true);
    // Renewals far apart, so only a wake could cause another request.
    state.pane_viewers.set_renew_every(Duration::from_secs(600));
    let _sink = viewer(&state, "viewer-a").await;
    until("the capture to open", || remote.opens() >= 1).await;

    // A session that never watched a pane holds no pane topic to report.
    let other = session_sink();
    state.topic_broker.register_session("cli", other).await;
    assert!(state.topic_broker.remove_session("cli").await.is_empty());
    for _ in 0..3 {
        state.pane_viewers.viewers_may_have_left().await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(remote.opens(), 1, "a departure is not a join");
    assert_eq!(remote.cancels(), 0);
    assert_eq!(
        state.topic_broker.remove_session("viewer-a").await,
        vec![pane_capture_topic(RUN)],
        "a viewer's departure names the pane topic it held"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replacement_capture_is_never_cancelled_by_the_task_it_replaced() {
    let remote = Remote::new(PaneCaptureOutcome::Accepted);
    let (state, _dir) = state_with(&remote, true);
    for round in 0..8 {
        let id = format!("viewer-{round}");
        let _sink = viewer(&state, &id).await;
        until("an open", || remote.opens() > round).await;
        let watched = remote.latest_subscription();
        // Leave and come straight back, as a closed and reopened panel does.
        unsubscribe(&state, &id).await;
        let again = format!("viewer-{round}-again");
        let _sink = viewer(&state, &again).await;
        // Long enough for a departing task's cancel and the replacement's
        // first open to both have reached the host.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let live = remote.latest_subscription();
        let cancelled: Vec<String> = remote
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.cancel)
            .map(|r| r.subscription_id.clone())
            .collect();
        assert!(
            !cancelled.contains(&live),
            "round {round}: the cancel of {watched} ended the live subscription {live}"
        );
        unsubscribe(&state, &again).await;
        until("the task to end", || {
            futures_executor_block(state.pane_viewers.active_runs()) == 0
        })
        .await;
    }
}
