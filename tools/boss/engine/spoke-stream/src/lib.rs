//! Authenticated hub transport, deliberately unwired from engine dispatch.
//!
//! Mount `StreamManager::service()` on a tonic TLS server trusting the host CA.
//! Connect still requires a peer certificate if enrollment shares an optional-
//! client-auth listener. Supply HostAuthorizer and the transactional contracts
//! in `store`; this crate owns no second database or execution policy.
mod codec;
mod commands;
mod service;
mod session;
pub mod store;

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use boss_host_enrollment::HostAuthorizer;
use boss_protocol_spoke::{negotiate::Contracts, v1::CommandResult, v1::Run};
use tokio::sync::{Mutex, Notify, Semaphore, broadcast, mpsc, oneshot};

pub use commands::CommandOutcome;
pub use service::StreamService;
use store::{CommandOutbox, SpokeObservationSink};

#[derive(Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct Config {
    pub hub_id: String,
    pub authority_generation: String,
    pub build_fingerprint: String,
    #[builder(default = 128)]
    pub max_hosts: usize,
    #[builder(default = 64)]
    pub max_pending: usize,
    #[builder(default = Duration::from_secs(10))]
    pub hello_timeout: Duration,
    #[builder(default = Duration::from_secs(5))]
    pub pulse_interval: Duration,
    #[builder(default = Duration::from_secs(15))]
    pub pulse_timeout: Duration,
    /// What this hub offers and requires of a spoke.
    #[builder(default = Contracts::first_release())]
    pub contracts: Contracts,
}

/// How a nonce challenge against the connection that holds a host ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChallengeOutcome {
    /// The live daemon answered before its challenge deadline.
    Answered,
    /// A challenge was already outstanding for another contender.
    Pending,
    /// The challenge deadline passed without an answer.
    Expired,
}

pub(crate) type Host = Arc<Mutex<Option<Connection>>>;
pub(crate) type Waiters = HashMap<String, oneshot::Sender<CommandResult>>;

#[derive(bon::Builder)]
pub(crate) struct Connection {
    session: store::Session,
    nonce: Vec<u8>,
    active: Arc<AtomicBool>,
    probe: mpsc::Sender<oneshot::Sender<ChallengeOutcome>>,
    terminal: Arc<std::sync::Mutex<Option<tonic::Status>>>,
    wake: Arc<Notify>,
    waiters: Waiters,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Still active means a newer claim (or a failed one) is fencing this
        // stream; a driver that ended on its own already cleared `active` and
        // recorded its own status. Publish the status before clearing `active`.
        if self.active.load(Ordering::Acquire) {
            self.terminal
                .lock()
                .expect("terminal lock")
                .get_or_insert_with(|| tonic::Status::aborted("superseded by a newer connection"));
        }
        self.active.store(false, Ordering::Release);
        self.wake.notify_one();
    }
}

pub(crate) struct Storage {
    sink: Arc<dyn SpokeObservationSink>,
    outbox: Arc<dyn CommandOutbox>,
}

#[derive(bon::Builder)]
pub(crate) struct Inner {
    config: Config,
    auth: Arc<dyn HostAuthorizer>,
    storage: Storage,
    hosts: Mutex<HashMap<String, Host>>,
    connections: Arc<Semaphore>,
    captures: broadcast::Sender<Arc<CapturedPane>>,
}

/// Snapshots a slow viewer feed may lag behind by before it skips the oldest.
const CAPTURE_FEED_CAPACITY: usize = 32;

/// One pane snapshot a spoke reported for a viewer's capture subscription.
///
/// Snapshots are live display data, not journal facts: the hub commits the
/// record like any other (receipt and sequence stay contiguous) and then hands
/// this to whoever is watching. Nothing replays it, and a feed that lags simply
/// skips to the newest.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct CapturedPane {
    pub host_id: String,
    pub run: Run,
    pub subscription_id: String,
    /// The spoke's text, exactly as received. UTF-8 is validated on ingress;
    /// terminal control characters are NOT removed, so a consumer that shows it
    /// must escape it.
    pub text: String,
    /// Older lines were dropped to fit the subscriber's byte budget.
    pub truncated: bool,
    /// The spoke's wall clock when it looked; display only.
    pub observed_unix_ms: i64,
    /// This hub's wall clock when the record was committed.
    pub received_unix_ms: i64,
}

#[derive(Clone)]
pub struct StreamManager(Arc<Inner>);

impl StreamManager {
    pub fn new(
        config: Config,
        auth: Arc<dyn HostAuthorizer>,
        sink: Arc<dyn SpokeObservationSink>,
        outbox: Arc<dyn CommandOutbox>,
    ) -> Self {
        assert!(config.max_hosts > 0 && config.max_pending > 0);
        assert!(!config.pulse_interval.is_zero() && !config.pulse_timeout.is_zero());
        assert!(!config.hub_id.is_empty() && !config.authority_generation.is_empty());
        let connections = Arc::new(Semaphore::new(config.max_hosts.saturating_mul(2)));
        let (captures, _) = broadcast::channel(CAPTURE_FEED_CAPACITY);
        Self(Arc::new(
            Inner::builder()
                .config(config)
                .auth(auth)
                .storage(Storage { sink, outbox })
                .hosts(Mutex::new(HashMap::new()))
                .connections(connections)
                .captures(captures)
                .build(),
        ))
    }

    /// Pane snapshots from every connected spoke, newest last. A receiver that
    /// falls behind sees `Lagged` and should carry on: only the latest snapshot
    /// per subscription matters.
    pub fn subscribe_captures(&self) -> broadcast::Receiver<Arc<CapturedPane>> {
        self.0.captures.subscribe()
    }

    /// Whether `host_id` has a live connection that negotiated `feature`.
    /// `None` when it is not connected, which is different from "connected
    /// without it": the first is worth retrying, the second is not.
    pub async fn host_supports(&self, host_id: &str, feature: &str) -> Option<bool> {
        let host = self.0.hosts.lock().await.get(host_id).cloned()?;
        let guard = host.lock().await;
        let connection = guard.as_ref().filter(|c| c.active.load(Ordering::Acquire))?;
        Some(connection.session.selection.has(feature))
    }

    pub(crate) fn publish_capture(&self, host_id: &str, record: &boss_protocol_spoke::v1::Record) {
        use boss_protocol_spoke::v1::record::Body;
        let (Some(Body::Capture(capture)), Some(run)) = (&record.body, &record.run) else {
            return;
        };
        if self.0.captures.receiver_count() == 0 {
            return;
        }
        let received = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        let _ = self.0.captures.send(Arc::new(
            CapturedPane::builder()
                .host_id(host_id)
                .run(run.clone())
                .subscription_id(&capture.subscription_id)
                .text(String::from_utf8_lossy(&capture.text_utf8))
                .truncated(capture.truncated)
                .observed_unix_ms(record.observed.as_ref().map_or(0, |stamp| stamp.wall_unix_ms))
                .received_unix_ms(received)
                .build(),
        ));
    }

    pub fn service(&self) -> StreamService {
        StreamService(self.clone())
    }

    pub(crate) async fn host(&self, id: &str) -> Result<Host, Box<tonic::Status>> {
        let mut hosts = self.0.hosts.lock().await;
        if let Some(host) = hosts.get(id) {
            return Ok(host.clone());
        }
        // Retain only active or currently acquired slots; no unbounded history.
        hosts.retain(|_, host| Arc::strong_count(host) > 1);
        if hosts.len() >= self.0.config.max_hosts {
            return Err(tonic::Status::resource_exhausted("host limit").into());
        }
        let host = Arc::new(Mutex::new(None));
        hosts.insert(id.into(), host.clone());
        Ok(host)
    }
}

pub(crate) fn status(error: store::StoreError) -> tonic::Status {
    use store::StoreError::*;
    match error {
        ReplayGap => tonic::Status::failed_precondition("replay_gap"),
        Denied => tonic::Status::permission_denied("not permitted"),
        Full => tonic::Status::resource_exhausted("durable quota exhausted"),
        Unavailable => tonic::Status::unavailable("durable store unavailable"),
    }
}
