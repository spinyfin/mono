//! Bounded frontend read admission. Internal work and mutations never enter
//! this lane. Acquire the connection permit before the engine permit so a
//! single pipelined connection cannot occupy the engine's whole wait queue.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Queued (not yet active) reads allowed per active permit on one connection.
const PENDING_PER_ACTIVE: usize = 4;

const QUEUE_FULL: &str = "engine busy, retry: read admission queue full";
const DEADLINE: &str = "engine busy, retry: read admission deadline expired";
mod diagnostics;
pub(super) use diagnostics::Identity;
use diagnostics::{Diagnostics, TrackedRead, Wait};

pub(super) struct ReadAdmission {
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    per_connection: usize,
    wait: Duration,
    diagnostics: Arc<Diagnostics>,
}

pub(super) struct ConnectionReads {
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
}

pub(super) struct WaitingRead {
    engine: Arc<ReadAdmission>,
    connection: Arc<ConnectionReads>,
    pending: (OwnedSemaphorePermit, OwnedSemaphorePermit),
    deadline: tokio::time::Instant,
    tracked: Option<TrackedRead>,
}

/// A read that missed its admission deadline, still holding its queue slot.
///
/// The slot is released when this is dropped, and the caller must enqueue
/// the busy reply *before* that. The reader loop's headroom gate
/// ([`reply_headroom`]) only counts replies it can see: queued envelopes
/// plus the reads these permits represent. If the slot were freed first,
/// the reader could admit another request into it while the busy reply is
/// still on its way to the queue, so outstanding replies would exceed the
/// reserve and a full lane of replies would disconnect the session.
pub(super) struct RejectedRead {
    message: &'static str,
    _pending: (OwnedSemaphorePermit, OwnedSemaphorePermit),
}

impl RejectedRead {
    pub(super) fn message(&self) -> &'static str {
        self.message
    }
}

impl std::fmt::Debug for RejectedRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectedRead").field("message", &self.message).finish()
    }
}

impl PartialEq<&str> for RejectedRead {
    fn eq(&self, other: &&str) -> bool {
        self.message == *other
    }
}

// Deliberately very high, pending measurement of normal traffic: the
// earlier defaults (32 active, 500 ms wait) rejected the macOS app's own
// startup reads after a cold engine start. These match the overrides that
// restored normal use. Lower them only from measured data, with a generous
// buffer above the observed peak. The env overrides below still apply.
impl Default for ReadAdmission {
    fn default() -> Self {
        Self::new(
            setting("BOSS_RPC_READ_CONCURRENCY", 4096),
            setting("BOSS_RPC_READ_PER_CONNECTION", 4096),
            setting("BOSS_RPC_READ_QUEUE", 100_000),
            Duration::from_millis(setting("BOSS_RPC_READ_WAIT_MS", 60_000) as u64),
        )
    }
}

fn setting(name: &str, default: usize) -> usize {
    let value = boss_engine_utils::env_parse::env_parsed_or(name, default);
    if value > 0 && value <= 1_000_000 {
        value
    } else {
        tracing::warn!(name, value, default, "invalid read admission setting; using default");
        default
    }
}

/// Outbound-queue depth the reader loop waits for before reading another
/// request: every reply both lanes may have outstanding, plus one, capped at
/// half the outbound queue. With the very high default limits the uncapped
/// value exceeds the queue, which would make the reader wait for a completely
/// empty queue before every request.
pub(super) fn reply_headroom(bulk: &ReadAdmission, live: &ReadAdmission) -> usize {
    bulk.outstanding_per_connection()
        .saturating_add(live.outstanding_per_connection())
        .saturating_add(1)
        .min(MAX_REPLY_RESERVE)
}

const MAX_REPLY_RESERVE: usize = super::session_queue::MAX_SESSION_QUEUE / 2;

impl ReadAdmission {
    pub(super) fn live() -> Self {
        Self::new(
            // Deliberately very high pending measurement; see the bulk defaults above.
            setting("BOSS_RPC_LIVE_READ_CONCURRENCY", 1024),
            setting("BOSS_RPC_LIVE_READ_PER_CONNECTION", 1024),
            setting("BOSS_RPC_LIVE_READ_QUEUE", 100_000),
            Duration::from_millis(setting("BOSS_RPC_LIVE_READ_WAIT_MS", 60_000) as u64),
        )
    }

    pub(super) fn new(global: usize, per_connection: usize, queue: usize, wait: Duration) -> Self {
        Self {
            active: Arc::new(Semaphore::new(global)),
            pending: Arc::new(Semaphore::new(queue)),
            per_connection,
            wait,
            diagnostics: Arc::default(),
        }
    }

    pub(super) fn outstanding_per_connection(&self) -> usize {
        self.per_connection.saturating_mul(PENDING_PER_ACTIVE + 1)
    }

    pub(super) fn connection(&self) -> Arc<ConnectionReads> {
        Arc::new(ConnectionReads {
            active: Arc::new(Semaphore::new(self.per_connection)),
            pending: Arc::new(Semaphore::new(self.per_connection.saturating_mul(PENDING_PER_ACTIVE))),
        })
    }

    /// Bound task allocation as well as active handlers. Full queues reject
    /// immediately; accepted waiters share one deadline across both limits.
    #[cfg(test)]
    pub(super) fn enqueue(self: &Arc<Self>, connection: &Arc<ConnectionReads>) -> Result<WaitingRead, &'static str> {
        self.enqueue_as(connection, None)
    }

    /// Like [`Self::enqueue`], naming the request so a queue-full dump can
    /// identify the rejected read, which is not yet tracked.
    pub(super) fn enqueue_as(
        self: &Arc<Self>,
        connection: &Arc<ConnectionReads>,
        identity: Option<Identity<'_>>,
    ) -> Result<WaitingRead, &'static str> {
        let full = |wait: Wait| {
            self.diagnostics
                .report(QUEUE_FULL, wait, None, identity, Arc::as_ptr(connection) as usize);
            QUEUE_FULL
        };
        let local = connection
            .pending
            .clone()
            .try_acquire_owned()
            .map_err(|_| full(Wait::ConnectionQueue))?;
        let global = self
            .pending
            .clone()
            .try_acquire_owned()
            .map_err(|_| full(Wait::EngineQueue))?;
        Ok(WaitingRead {
            engine: self.clone(),
            connection: connection.clone(),
            pending: (global, local),
            deadline: tokio::time::Instant::now() + self.wait,
            tracked: None,
        })
    }
}

impl WaitingRead {
    /// `key` is `view::key_id` of the request.
    pub(super) fn identify(mut self, session: &str, request: &str, verb: String, key: u64) -> Self {
        self.tracked = Some(
            self.engine
                .diagnostics
                .track(session, request, verb, key, self.connection_id()),
        );
        self
    }

    fn connection_id(&self) -> usize {
        Arc::as_ptr(&self.connection) as usize
    }

    /// Wait for the active permits. The queue slot moves into the returned
    /// [`RejectedRead`] on failure rather than being released here; see its
    /// documentation for why the busy reply must be queued before it drops.
    pub(super) async fn acquire(self) -> Result<ReadPermits, RejectedRead> {
        let result = tokio::time::timeout_at(self.deadline, async {
            if let Some(read) = &self.tracked {
                read.stage("connection permit");
            }
            let local = self
                .connection
                .active
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| DEADLINE)?;
            if let Some(read) = &self.tracked {
                read.stage("engine permit");
            }
            let global = self.engine.active.clone().acquire_owned().await.map_err(|_| DEADLINE)?;
            Ok((local, global))
        })
        .await
        .unwrap_or(Err(DEADLINE));
        match result {
            Ok(permits) => {
                if let Some(read) = &self.tracked {
                    read.stage("blocking queue");
                }
                Ok(ReadPermits {
                    _permits: permits,
                    tracked: self.tracked,
                })
            }
            Err(message) => {
                self.engine.diagnostics.report(
                    message,
                    Wait::Permit,
                    self.tracked.as_ref(),
                    None,
                    self.connection_id(),
                );
                Err(RejectedRead {
                    message,
                    _pending: self.pending,
                })
            }
        }
    }
}

mod lane;
pub(super) use lane::{ReadLane, read_lane};

mod view;
pub(super) use view::{key, key_id};

#[cfg(test)]
mod burst_tests;
#[cfg(test)]
mod contention_tests;
#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) struct ReadPermits {
    _permits: (OwnedSemaphorePermit, OwnedSemaphorePermit),
    tracked: Option<TrackedRead>,
}

impl ReadPermits {
    pub(super) fn started(&self) {
        if let Some(read) = &self.tracked {
            read.stage("handler");
        }
    }
}
