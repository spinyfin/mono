//! Read-only remote pane viewer (hub-and-spoke design §2.8).
//!
//! A session that subscribes to a run's [`pane_capture_topic`] is a viewer.
//! While at least one viewer is present the engine keeps one short-lived
//! capture subscription alive on the worker's host, renewing it before its TTL
//! passes, and turns each snapshot the host reports into a
//! [`TopicEventPayload::WorkerPaneCapture`] push. When the last viewer
//! unsubscribes, disconnects or is evicted the engine cancels the capture; if
//! it cannot (the host is away, the engine dies) the host's TTL ends it
//! anyway, so nothing here needs to be remembered across a restart.
//!
//! This is text, not a terminal: the pane is never written to, and the text
//! pushed to viewers has its control characters turned into visible escapes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use boss_execution_contracts::{PaneCaptureOutcome, PaneCaptureRequest};
use boss_protocol::{FrontendEvent, FrontendEventEnvelope, PaneCaptureState, TopicEventPayload, pane_capture_topic};
use tokio::sync::{Mutex, Notify, broadcast};

use super::ServerState;

/// How often a live capture is renewed. Well inside the host's default 30
/// second TTL, so one lost renewal does not end the view.
const RENEW_EVERY: Duration = Duration::from_secs(10);
/// A host that does not answer a capture request is treated as unreachable.
const REQUEST_DEADLINE: Duration = Duration::from_secs(15);
/// The cancel on the way out is a courtesy: the TTL is the real stop.
const CANCEL_DEADLINE: Duration = Duration::from_secs(3);
/// Snapshots a slow task may lag behind before it skips to the newest.
const FEED_CAPACITY: usize = 32;

/// One snapshot of a worker's pane as the host reported it, before it is made
/// safe to show.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct PaneSnapshot {
    pub run_id: String,
    pub host_id: String,
    pub subscription_id: String,
    pub text: String,
    pub truncated: bool,
    pub observed_unix_ms: i64,
}

/// The capture subscription id one capture task uses for a run, shared by every
/// viewer of it. Each task gets its own, so a departing task's cancel can never
/// end the subscription of the task that replaced it.
fn subscription_id(run_id: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("viewer-{run_id}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// How a capture task is woken. A join asks for an immediate request to the
/// host; a departure only asks the task to re-check whether anyone is left.
#[derive(Default)]
struct TaskWake {
    notify: Notify,
    joined: AtomicBool,
}

pub(super) struct PaneViewers {
    feed: broadcast::Sender<Arc<PaneSnapshot>>,
    /// Runs with a live capture task. Held across the task's decision to exit
    /// so a viewer that arrives at that instant starts a fresh task.
    running: Mutex<HashMap<String, Arc<TaskWake>>>,
    renew_every_ms: AtomicU64,
}

impl Default for PaneViewers {
    fn default() -> Self {
        Self {
            feed: broadcast::channel(FEED_CAPACITY).0,
            running: Mutex::new(HashMap::new()),
            renew_every_ms: AtomicU64::new(RENEW_EVERY.as_millis() as u64),
        }
    }
}

impl PaneViewers {
    /// Where snapshots from hosts enter. The hub stream feeds it in
    /// production; a test feeds it directly.
    pub(super) fn feed(&self) -> broadcast::Sender<Arc<PaneSnapshot>> {
        self.feed.clone()
    }

    #[cfg(test)]
    pub(super) fn set_renew_every(&self, every: Duration) {
        self.renew_every_ms.store(every.as_millis() as u64, Ordering::SeqCst);
    }

    /// Runs with a capture task, for tests that assert a viewer's departure
    /// really stopped it.
    #[cfg(test)]
    pub(super) async fn active_runs(&self) -> usize {
        self.running.lock().await.len()
    }

    /// A session subscribed to `run_id`'s pane topic: make sure a capture task
    /// is running for it.
    pub(super) async fn viewer_joined(&self, state: &Arc<ServerState>, run_id: &str) {
        let mut running = self.running.lock().await;
        if let Some(wake) = running.get(run_id) {
            wake.joined.store(true, Ordering::SeqCst);
            wake.notify.notify_one();
            return;
        }
        let wake = Arc::new(TaskWake::default());
        running.insert(run_id.to_owned(), Arc::clone(&wake));
        let state = Arc::clone(state);
        let run_id = run_id.to_owned();
        boss_task_scope::spawn(async move { capture_task(state, run_id, wake).await });
    }

    /// Some viewer left (unsubscribed, disconnected or was evicted): every task
    /// re-checks whether anyone is still watching. This is not a join, so a
    /// task with viewers left goes back to waiting without a request.
    pub(super) async fn viewers_may_have_left(&self) {
        for wake in self.running.lock().await.values() {
            wake.notify.notify_one();
        }
    }
}

/// Keeps `run_id`'s capture alive while someone watches it.
async fn capture_task(state: Arc<ServerState>, run_id: String, wake: Arc<TaskWake>) {
    let topic = pane_capture_topic(&run_id);
    let sub_id = subscription_id(&run_id);
    let mut snapshots = state.pane_viewers.feed.subscribe();
    let renew_every = Duration::from_millis(state.pane_viewers.renew_every_ms.load(Ordering::SeqCst).max(1));
    let mut renew = tokio::time::interval(renew_every);
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is immediate; the first request is made below.
    renew.tick().await;
    let mut host_id = String::new();
    let mut adapter: Option<Arc<dyn boss_execution_contracts::HostAdapter>> = None;
    // What viewers were last told about the capture's health, so an unchanged
    // failure is not re-pushed on every renewal.
    let mut told: Option<(PaneCaptureState, String)> = None;
    // Last Live snapshot this task published. A late joiner or a recovery from
    // a health banner is replayed from here so they do not wait for the spoke's
    // keep-alive. Cleared only by a newer snapshot; a health/terminal publish
    // supersedes it without dropping it.
    let mut last_live: Option<Arc<PaneSnapshot>> = None;
    let mut first = true;

    loop {
        let mut due = first;
        if !first {
            tokio::select! {
                _ = renew.tick() => { due = true; }
                _ = wake.notify.notified() => {}
                snapshot = snapshots.recv() => {
                    match snapshot {
                        Ok(snapshot) if snapshot.run_id == run_id && snapshot.subscription_id == sub_id => {
                            told = None;
                            last_live = Some(Arc::clone(&snapshot));
                            publish_snapshot(&state, &snapshot).await;
                        }
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                    continue;
                }
            }
        }
        let joined = std::mem::take(&mut first) || wake.joined.swap(false, Ordering::SeqCst);

        if !state.topic_broker.has_subscribers(&topic).await {
            // Leaving is decided under the registry lock, against a fresh
            // look, so a viewer arriving now is not left without a task.
            let mut running = state.pane_viewers.running.lock().await;
            if state.topic_broker.has_subscribers(&topic).await {
                continue;
            }
            running.remove(&run_id);
            drop(running);
            if let Some(adapter) = &adapter {
                cancel(adapter.as_ref(), &run_id, &sub_id).await;
            }
            return;
        }
        if joined
            && told.is_none()
            && let Some(cached) = &last_live
        {
            publish_snapshot(&state, cached).await;
        }
        if !due && !joined {
            // Only a departure woke the task, and a viewer remains.
            continue;
        }

        if adapter.is_none() {
            match state.spoke_worker_adapter(&run_id).await {
                Ok(Some(resolved)) => {
                    host_id = resolved.host_id().to_owned();
                    adapter = Some(resolved);
                }
                Ok(None) => {
                    let (status, detail) = match state.work_db.spoke_assignment_for_execution(&run_id) {
                        Ok(Some(assignment)) if assignment.resolved => {
                            (PaneCaptureState::Ended, "the worker has ended".to_owned())
                        }
                        Ok(_) => (
                            PaneCaptureState::Unsupported,
                            "pane capture is only available for workers on spoke-connected hosts".to_owned(),
                        ),
                        Err(error) => (
                            PaneCaptureState::Unreachable,
                            format!("could not read the worker's assignment: {error:#}"),
                        ),
                    };
                    tell_if_new(&state, &run_id, &host_id, &mut told, status, detail, joined).await;
                    if finish_if_unwatched(&state, &run_id, &topic).await {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    tell_if_new(
                        &state,
                        &run_id,
                        &host_id,
                        &mut told,
                        PaneCaptureState::Unreachable,
                        format!("could not reach the worker's host: {error:#}"),
                        joined,
                    )
                    .await;
                    continue;
                }
            }
        }
        let Some(host) = adapter.as_ref() else { continue };

        let request = PaneCaptureRequest {
            subscription_id: sub_id.clone(),
            interval_ms: 0,
            max_bytes: 0,
            ttl_seconds: 0,
            cancel: false,
        };
        let outcome = match tokio::time::timeout(REQUEST_DEADLINE, host.capture_worker_pane(&run_id, &request)).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => PaneCaptureOutcome::Unreachable(format!("{error:#}")),
            Err(_) => PaneCaptureOutcome::Unreachable("the host did not answer in time".to_owned()),
        };
        match outcome {
            // Snapshots arrive on the feed. A join or renewal that recovered
            // from a health/terminal banner republishes the last Live text so
            // viewers are not blank until the spoke's keep-alive.
            PaneCaptureOutcome::Accepted => {
                if told.take().is_some()
                    && let Some(cached) = &last_live
                {
                    publish_snapshot(&state, cached).await;
                }
            }
            PaneCaptureOutcome::Ended => {
                tell_if_new(
                    &state,
                    &run_id,
                    &host_id,
                    &mut told,
                    PaneCaptureState::Ended,
                    "the worker has ended".to_owned(),
                    joined,
                )
                .await;
                if finish_if_unwatched(&state, &run_id, &topic).await {
                    return;
                }
            }
            PaneCaptureOutcome::Unsupported(reason) => {
                tell_if_new(
                    &state,
                    &run_id,
                    &host_id,
                    &mut told,
                    PaneCaptureState::Unsupported,
                    reason,
                    joined,
                )
                .await;
            }
            PaneCaptureOutcome::Unreachable(reason) => {
                tell_if_new(
                    &state,
                    &run_id,
                    &host_id,
                    &mut told,
                    PaneCaptureState::Unreachable,
                    reason,
                    joined,
                )
                .await;
            }
            PaneCaptureOutcome::Refused(reason) => {
                tell_if_new(
                    &state,
                    &run_id,
                    &host_id,
                    &mut told,
                    PaneCaptureState::Refused,
                    reason,
                    joined,
                )
                .await;
            }
        }
    }
    state.pane_viewers.running.lock().await.remove(&run_id);
}

/// Drop the task from the registry only when nobody is watching. A viewer that
/// arrives between a terminal publish and this check is attached to a live
/// task rather than one that is about to return.
async fn finish_if_unwatched(state: &ServerState, run_id: &str, topic: &str) -> bool {
    let mut running = state.pane_viewers.running.lock().await;
    if state.topic_broker.has_subscribers(topic).await {
        return false;
    }
    running.remove(run_id);
    true
}

async fn cancel(adapter: &dyn boss_execution_contracts::HostAdapter, run_id: &str, sub_id: &str) {
    let request = PaneCaptureRequest {
        subscription_id: sub_id.to_owned(),
        interval_ms: 0,
        max_bytes: 0,
        ttl_seconds: 0,
        cancel: true,
    };
    match tokio::time::timeout(CANCEL_DEADLINE, adapter.capture_worker_pane(run_id, &request)).await {
        Ok(Ok(outcome)) => tracing::debug!(run_id, ?outcome, "cancelled a pane capture"),
        Ok(Err(error)) => {
            tracing::debug!(run_id, error = %format!("{error:#}"), "could not cancel a pane capture; its TTL will")
        }
        Err(_) => tracing::debug!(run_id, "pane capture cancel timed out; its TTL will end it"),
    }
}

async fn tell_if_new(
    state: &ServerState,
    run_id: &str,
    host_id: &str,
    told: &mut Option<(PaneCaptureState, String)>,
    capture_state: PaneCaptureState,
    detail: String,
    force: bool,
) {
    let key = (capture_state, detail);
    if !force && told.as_ref() == Some(&key) {
        return;
    }
    publish_state(state, run_id, host_id, key.0, &key.1).await;
    *told = Some(key);
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

async fn publish(state: &ServerState, run_id: &str, payload: TopicEventPayload) {
    let topic = pane_capture_topic(run_id);
    // Display data, not board state: it must not move the work revision.
    let event = FrontendEvent::TopicEvent {
        topic: topic.clone(),
        revision: state.current_work_revision(),
        origin_session_id: String::new(),
        origin_request_id: None,
        event: payload,
    };
    state
        .topic_broker
        .publish(&topic, FrontendEventEnvelope::push(event))
        .await;
}

async fn publish_snapshot(state: &ServerState, snapshot: &PaneSnapshot) {
    let payload = TopicEventPayload::WorkerPaneCapture {
        run_id: snapshot.run_id.clone(),
        host_id: snapshot.host_id.clone(),
        state: PaneCaptureState::Live,
        text: escape_terminal_text(&snapshot.text),
        truncated: snapshot.truncated,
        observed_unix_ms: Some(snapshot.observed_unix_ms).filter(|ms| *ms > 0),
        received_unix_ms: now_unix_ms(),
        detail: None,
    };
    publish(state, &snapshot.run_id, payload).await;
}

async fn publish_state(
    state: &ServerState,
    run_id: &str,
    host_id: &str,
    capture_state: PaneCaptureState,
    detail: &str,
) {
    let payload = TopicEventPayload::WorkerPaneCapture {
        run_id: run_id.to_owned(),
        host_id: host_id.to_owned(),
        state: capture_state,
        text: String::new(),
        truncated: false,
        observed_unix_ms: None,
        received_unix_ms: now_unix_ms(),
        detail: Some(detail.to_owned()),
    };
    publish(state, run_id, payload).await;
}

/// Make worker-controlled text safe to display: every control character other
/// than newline and tab (ESC, BEL, backspace, carriage return, the C1 range,
/// DEL) is replaced by a visible `\xNN` / `\u{…}` escape, so a pane that prints
/// terminal sequences shows them as text and can never act on the viewer.
pub(super) fn escape_terminal_text(raw: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            // C1 controls, line/paragraph separators and bidi overrides.
            c if matches!(c as u32, 0x80..=0x9f | 0x2028 | 0x2029 | 0x202a..=0x202e | 0x2066..=0x2069) => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Pass the hub stream's pane snapshots on to the viewers' feed. Ends when the
/// stream's manager is dropped.
pub(super) fn bridge_hub_stream(state: &Arc<ServerState>, stream: &boss_engine_runtime::hub::HubStream) {
    let mut captures = stream.manager().subscribe_captures();
    let feed = state.pane_viewers.feed();
    boss_task_scope::spawn(async move {
        loop {
            match captures.recv().await {
                Ok(captured) => {
                    let _ = feed.send(Arc::new(
                        PaneSnapshot::builder()
                            .run_id(&captured.run.execution_id)
                            .host_id(&captured.host_id)
                            .subscription_id(&captured.subscription_id)
                            .text(&captured.text)
                            .truncated(captured.truncated)
                            .observed_unix_ms(captured.observed_unix_ms)
                            .build(),
                    ));
                }
                // Only the newest snapshot matters.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_become_visible_text() {
        assert_eq!(escape_terminal_text("ok\n\tfine"), "ok\n\tfine");
        assert_eq!(
            escape_terminal_text("\x1b[31mred\x07\x08\r\x7f"),
            "\\x1b[31mred\\x07\\x08\\x0d\\x7f"
        );
        assert_eq!(
            escape_terminal_text("a\u{9b}31m\u{202e}b\u{2028}"),
            "a\\u{9b}31m\\u{202e}b\\u{2028}"
        );
        assert_eq!(escape_terminal_text("naïve — 日本語 🙂"), "naïve — 日本語 🙂");
        assert!(
            escape_terminal_text("\x1b]0;title\x07")
                .chars()
                .all(|c| !c.is_control())
        );
    }
}

#[cfg(test)]
mod flow_tests {
    //! The read-only remote pane viewer through the real subscribe / unsubscribe
    //! handlers and the topic broker (hub-and-spoke design §2.8).
    use super::PaneSnapshot;
    use crate::app::{
        Dispatch, ServerStateOverrides, SessionSink, subscriptions, tests::test_server_state_with_overrides,
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use boss_execution_contracts::*;
    use boss_protocol::{
        FrontendEvent, FrontendRequest, PaneCaptureState, TopicEventPayload, WorkExecution, WorkItem,
        pane_capture_topic,
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

    fn state_with(remote: &Arc<Remote>, spoke_run: bool) -> (Arc<crate::app::ServerState>, tempfile::TempDir) {
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
    async fn viewer(state: &Arc<crate::app::ServerState>, id: &str) -> Arc<SessionSink> {
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

    async fn unsubscribe(state: &Arc<crate::app::ServerState>, id: &str) {
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
    async fn a_second_viewer_gets_the_last_snapshot_without_a_new_host_snapshot() {
        let remote = Remote::new(PaneCaptureOutcome::Accepted);
        let (state, _dir) = state_with(&remote, true);
        state.pane_viewers.set_renew_every(Duration::from_secs(600));
        let first = viewer(&state, "viewer-a").await;
        until("the capture to open", || remote.opens() >= 1).await;

        state.pane_viewers.feed().send(snapshot(&remote, "idle pane")).unwrap();
        assert_eq!(next_capture(&first).await.1, "idle pane");

        let second = viewer(&state, "viewer-b").await;
        let envelope = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let envelope = second.next().await.expect("sink closed");
                if let FrontendEvent::TopicEvent {
                    event:
                        TopicEventPayload::WorkerPaneCapture {
                            state: PaneCaptureState::Live,
                            text,
                            observed_unix_ms,
                            truncated,
                            ..
                        },
                    ..
                } = envelope.payload
                {
                    return (text, observed_unix_ms, truncated);
                }
            }
        })
        .await
        .expect("second viewer did not receive the cached pane");
        assert_eq!(envelope, ("idle pane".into(), Some(1_700_000_000_000), true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_join_does_not_replay_live_text_after_the_host_became_unreachable() {
        let remote = Remote::new(PaneCaptureOutcome::Accepted);
        let (state, _dir) = state_with(&remote, true);
        let first = viewer(&state, "viewer-a").await;
        until("the capture to open", || remote.opens() >= 1).await;
        state.pane_viewers.feed().send(snapshot(&remote, "stale")).unwrap();
        assert_eq!(next_capture(&first).await.1, "stale");

        *remote.answer.lock().unwrap() = PaneCaptureOutcome::Unreachable("host spoke is not connected".into());
        assert_eq!(next_capture(&first).await.0, PaneCaptureState::Unreachable);

        let second = viewer(&state, "viewer-b").await;
        let (capture_state, text, ..) = next_capture(&second).await;
        assert_eq!(capture_state, PaneCaptureState::Unreachable);
        assert!(text.is_empty(), "a superseded Live snapshot must not be replayed");
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
        assert!(
            futures_executor_block(state.pane_viewers.active_runs()) > 0,
            "the still-subscribed viewer keeps the task so a later recovery is not lost"
        );
        unsubscribe(&state, "viewer-a").await;
        until("the task to end", || {
            futures_executor_block(state.pane_viewers.active_runs()) == 0
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_that_answers_absent_then_accepted_keeps_the_view() {
        let remote = Remote::new(PaneCaptureOutcome::Unreachable("no live worker for this run".into()));
        let (state, _dir) = state_with(&remote, true);
        let assignment = state.work_db.spoke_assignment_for_execution(RUN).unwrap().unwrap();
        assert!(!assignment.resolved, "the assignment is still open");
        let sink = viewer(&state, "viewer-a").await;
        assert_eq!(next_capture(&sink).await.0, PaneCaptureState::Unreachable);
        assert!(
            futures_executor_block(state.pane_viewers.active_runs()) > 0,
            "a worker that has not spawned yet must not end the view"
        );

        *remote.answer.lock().unwrap() = PaneCaptureOutcome::Accepted;
        let opened = remote.opens();
        until("the next renewal", || remote.opens() > opened).await;
        state.pane_viewers.feed().send(snapshot(&remote, "started")).unwrap();
        let (capture_state, text, ..) = next_capture(&sink).await;
        assert_eq!((capture_state, text.as_str()), (PaneCaptureState::Live, "started"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_already_resolved_spoke_assignment_ends_the_view() {
        let remote = Remote::new(PaneCaptureOutcome::Accepted);
        let (state, _dir) = state_with(&remote, true);
        let assignment = state.work_db.spoke_assignment_for_execution(RUN).unwrap().unwrap();
        assert!(
            state
                .work_db
                .resolve_spoke_assignment(RUN, assignment.assignment_generation, "released")
                .unwrap()
        );
        let sink = viewer(&state, "viewer-a").await;
        assert_eq!(next_capture(&sink).await.0, PaneCaptureState::Ended);
        assert!(remote.requests.lock().unwrap().is_empty());
        unsubscribe(&state, "viewer-a").await;
        until("the task to end", || {
            futures_executor_block(state.pane_viewers.active_runs()) == 0
        })
        .await;
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
        unsubscribe(&state, "viewer-a").await;
        until("the task to end", || {
            futures_executor_block(state.pane_viewers.active_runs()) == 0
        })
        .await;
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
}
