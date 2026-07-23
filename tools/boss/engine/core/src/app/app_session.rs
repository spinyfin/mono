//! The registered app session and the engine→app RPC channel.
//!
//! Exactly one macOS app session is registered at a time. This module owns
//! that registration ([`ServerState::register_app_session`]), the outbound
//! request path ([`ServerState::send_to_app`]) with its pending-request
//! correlation map, the inbound response router
//! ([`ServerState::deliver_app_response`]), and the
//! [`AppChannelHealth`] liveness signal that surfaces a wedged push channel
//! as a single engine-health issue.
//!
//! Split out of `app.rs`; pure structural move — no behavioural change.

use super::*;

/// Live state for the registered app session. The sink is used to
/// push `EngineRequest` events; the pending map keys outstanding
/// engine→app calls by their `request_id`.
pub(super) struct AppSessionHandle {
    pub(super) session_id: String,
    sink: Arc<SessionSink>,
    pending: HashMap<String, oneshot::Sender<EngineToAppResponse>>,
    next_request_id: u64,
}

impl AppSessionHandle {
    pub(super) fn new(session_id: String, sink: Arc<SessionSink>) -> Self {
        Self {
            session_id,
            sink,
            pending: HashMap::new(),
            next_request_id: 1,
        }
    }

    pub(super) fn allocate_request_id(&mut self) -> String {
        let id = format!("eng-req-{}", self.next_request_id);
        self.next_request_id += 1;
        id
    }
}

/// Consecutive engine→app send failures after which the push channel is
/// treated as unhealthy and surfaced in the engine-health report. The
/// observed `reveal_work_item` incident failed two RPCs back-to-back, so
/// two in a row already indicates a saturated channel rather than a
/// one-off slow reply.
pub(super) const APP_CHANNEL_UNHEALTHY_STREAK: u64 = 2;

/// Liveness signal for the engine→app push channel. Updated by every
/// [`ServerState::send_to_app`] attempt and read synchronously by
/// [`build_engine_health_report`]. Atomics (not a mutex) so the health
/// path never contends with the hot send path or the `app_session` lock,
/// and so a single wedged channel produces one visible health signal
/// instead of a stream of per-call WARNs.
#[derive(Default)]
pub(super) struct AppChannelHealth {
    /// Consecutive send failures (timeout or undeliverable enqueue) since
    /// the last successful round-trip. Reset to 0 on any success.
    consecutive_failures: AtomicU64,
    /// Queue depth observed at the most recent failure (health-body/log only).
    last_queue_depth: AtomicU64,
    /// Head-of-line envelope age (ms) observed at the most recent failure.
    last_oldest_age_ms: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AppChannelHealthSnapshot {
    pub(super) consecutive_failures: u64,
    pub(super) last_queue_depth: u64,
    pub(super) last_oldest_age_ms: u64,
}

impl AppChannelHealth {
    pub(super) fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Record a failed send along with the queue stats observed at failure
    /// time. Returns the new consecutive-failure count.
    pub(super) fn record_failure(&self, stats: &QueueStats) -> u64 {
        self.last_queue_depth.store(stats.depth as u64, Ordering::Relaxed);
        self.last_oldest_age_ms.store(stats.oldest_age_ms, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(super) fn snapshot(&self) -> AppChannelHealthSnapshot {
        AppChannelHealthSnapshot {
            consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
            last_queue_depth: self.last_queue_depth.load(Ordering::Relaxed),
            last_oldest_age_ms: self.last_oldest_age_ms.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SendToAppError {
    #[error("no app session is registered")]
    NotRegistered,
    #[error("app session outbound queue is saturated; request could not be delivered")]
    SessionWedged,
    #[error("app disconnected before responding")]
    AppDisconnected,
    #[error("timed out waiting for app response")]
    Timeout,
    #[error("app responded with unexpected response kind for request kind {0}")]
    ResponseKindMismatch(&'static str),
}

impl ServerState {
    /// Send a request to the registered app session and await the
    /// response. Returns `Err` if no app is registered
    /// ([`SendToAppError::NotRegistered`]), the outbound queue is
    /// saturated so the request can't be delivered
    /// ([`SendToAppError::SessionWedged`]), the app disconnects before
    /// replying, or the request times out.
    ///
    /// The enqueue outcome is honoured: a `Slow`/`Closed` sink means the
    /// push can never reach the app, so we fail fast with a distinct error
    /// and tear the wedged session down (a reconnecting app then
    /// re-registers cleanly) rather than waiting out the full `wait` for a
    /// reply that will never come. Timeouts and undeliverable enqueues
    /// both feed [`ServerState::app_channel_health`] so a saturated
    /// channel surfaces as one engine-health issue.
    pub async fn send_to_app(
        &self,
        request: EngineToAppRequest,
        wait: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        let (tx, rx) = oneshot::channel();
        // Sink clone kept alongside the request id so we can read queue
        // stats on a timeout without re-locking the app_session mutex (the
        // registered handle may have been replaced by then).
        let (request_id, sink) = {
            let mut guard = self.app_session.lock().await;
            let Some(handle) = guard.as_mut() else {
                return Err(SendToAppError::NotRegistered);
            };
            let request_id = handle.allocate_request_id();
            handle.pending.insert(request_id.clone(), tx);
            let sink = handle.sink.clone();
            let outcome = sink.enqueue(FrontendEventEnvelope::push(FrontendEvent::EngineRequest {
                request_id: request_id.clone(),
                request: request.clone(),
            }));
            if matches!(outcome, EnqueueOutcome::Slow | EnqueueOutcome::Closed) {
                // Undeliverable: the outbound queue is saturated or closed.
                // Drop the pending entry (no reply will arrive), record the
                // health signal, log the saturation with queue stats, and
                // tear the wedged session down so the reader loop exits and
                // a reconnecting app re-registers atomically.
                handle.pending.remove(&request_id);
                let stats = sink.queue_stats();
                let session_id = handle.session_id.clone();
                drop(guard);
                let streak = self.app_channel_health.record_failure(&stats);
                tracing::warn!(
                    session_id = %session_id,
                    request_id = %request_id,
                    outcome = ?outcome,
                    queue_depth = stats.depth,
                    priority_depth = stats.priority_depth,
                    oldest_age_ms = stats.oldest_age_ms,
                    consecutive_failures = streak,
                    "send_to_app: app outbound queue saturated — request undeliverable; \
                     tearing down wedged app session",
                );
                sink.close();
                sink.trigger_shutdown();
                return Err(SendToAppError::SessionWedged);
            }
            (request_id, sink)
        };

        self.ipc_logger.log_request(&request_id, &request);

        match timeout(wait, rx).await {
            Ok(Ok(response)) => {
                self.app_channel_health.record_success();
                Ok(response)
            }
            Ok(Err(_recv_err)) => {
                self.drop_pending(&request_id).await;
                Err(SendToAppError::AppDisconnected)
            }
            Err(_elapsed) => {
                self.drop_pending(&request_id).await;
                // The request was enqueued but no reply arrived before the
                // deadline — the classic head-of-line-blocked drain. Record
                // the health signal and log the queue stats so the
                // saturation is diagnosable instead of a bare `Send(Timeout)`.
                let stats = sink.queue_stats();
                let streak = self.app_channel_health.record_failure(&stats);
                tracing::warn!(
                    request_id = %request_id,
                    queue_depth = stats.depth,
                    priority_depth = stats.priority_depth,
                    oldest_age_ms = stats.oldest_age_ms,
                    slow = stats.slow,
                    consecutive_failures = streak,
                    "send_to_app: timed out waiting for app response",
                );
                Err(SendToAppError::Timeout)
            }
        }
    }

    async fn drop_pending(&self, request_id: &str) {
        if let Some(handle) = self.app_session.lock().await.as_mut() {
            handle.pending.remove(request_id);
        }
    }

    /// Register `session_id` as the app session, atomically replacing any
    /// prior registration. The prior registration's pending requests are
    /// resolved as `AppDisconnected`. Logged so an app relaunch (or a
    /// re-register that displaces a wedged session) is attributable in the
    /// engine trace — see the incident where the app session silently
    /// vanished and every spawn failed "no app session is registered".
    pub(super) async fn register_app_session(&self, session_id: String, sink: Arc<SessionSink>) {
        let prior = self
            .app_session
            .lock()
            .await
            .replace(AppSessionHandle::new(session_id.clone(), sink));
        // A fresh registration means whatever channel-health streak the old
        // session accumulated no longer applies.
        self.app_channel_health.record_success();
        match &prior {
            Some(prior) => tracing::info!(
                session_id = %session_id,
                prior_session_id = %prior.session_id,
                dropped_pending = prior.pending.len(),
                "app session registered — replaced prior registration",
            ),
            None => tracing::info!(session_id = %session_id, "app session registered"),
        }
        if let Some(prior) = prior {
            for (_, tx) in prior.pending {
                let _ = tx.send(EngineToAppResponse::SpawnWorkerPane {
                    result: Err(EngineToAppError::AppDisconnected),
                });
            }
        }
    }

    /// If `session_id` is the registered app, drop the registration
    /// and resolve all pending requests as `AppDisconnected`. Called from
    /// the frontend reader loop's cleanup, so a logged drop here pins the
    /// exact moment (and cause: connection closed / reader-loop exit) the
    /// engine lost its app session.
    pub(super) async fn drop_app_session_if_matches(&self, session_id: &str) {
        let mut guard = self.app_session.lock().await;
        let take = matches!(guard.as_ref(), Some(handle) if handle.session_id == session_id);
        if take && let Some(prior) = guard.take() {
            drop(guard);
            tracing::warn!(
                session_id = %session_id,
                dropped_pending = prior.pending.len(),
                "app session dropped — frontend connection closed (reader-loop exit); \
                 engine→app RPCs will report NotRegistered until the app reconnects",
            );
            for (_, tx) in prior.pending {
                let _ = tx.send(EngineToAppResponse::SpawnWorkerPane {
                    result: Err(EngineToAppError::AppDisconnected),
                });
            }
        }
    }

    /// Snapshot the registered app session's outbound queue stats, if an
    /// app session is registered. Used by the periodic queue-depth logger.
    pub(super) async fn app_session_queue_stats(&self) -> Option<QueueStats> {
        self.app_session
            .lock()
            .await
            .as_ref()
            .map(|handle| handle.sink.queue_stats())
    }

    /// Route an `EngineResponse` from the app back to the waiting
    /// `send_to_app` caller.
    pub(super) async fn deliver_app_response(&self, session_id: &str, request_id: &str, response: EngineToAppResponse) {
        self.ipc_logger.log_response(request_id, &response);

        let mut guard = self.app_session.lock().await;
        let Some(handle) = guard.as_mut() else {
            tracing::warn!(request_id, "engine_response dropped: no registered app session",);
            return;
        };
        if handle.session_id != session_id {
            tracing::warn!(request_id, "engine_response dropped: came from non-app session",);
            return;
        }
        match handle.pending.remove(request_id) {
            Some(tx) => {
                let _ = tx.send(response);
            }
            None => {
                // The app answered a request whose `send_to_app` caller has
                // already given up (timed out or was cancelled). This is the
                // late-drain signature: the push sat behind bulk traffic,
                // the 5s send deadline elapsed, and the app's now-stale reply
                // has nowhere to go. Call it out so it isn't mistaken for a
                // protocol bug.
                tracing::warn!(
                    request_id,
                    "engine_response dropped: no pending request matches \
                     (caller already timed out — engine→app push likely drained late)",
                );
            }
        }
    }

    pub(super) fn allocate_session_id(&self) -> String {
        format!("session-{}", self.next_session_id.fetch_add(1, Ordering::Relaxed))
    }
}
