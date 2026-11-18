//! Bounded frontend read admission. Internal work and mutations never enter
//! this lane. Acquire the connection permit before the engine permit so a
//! single pipelined connection cannot occupy the engine's whole wait queue.
//!
//! Admission bounds concurrency; it never turns waiting into a client
//! error. A read that finds its connection's queue full waits — the reader
//! loop stops taking requests off that socket until a queue slot frees, which
//! is the backpressure. The engine-wide queue and the active permits are
//! waited for in the read's own task, so one session filling the engine-wide
//! queue never stops another session's reader loop from serving its writes
//! and live-lane reads. A wait longer than the lane's slow-wait threshold
//! logs the reads holding it up so the cause stays visible to operators.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Queued (not yet active) reads allowed per active permit on one connection.
const PENDING_PER_ACTIVE: usize = 4;

const SLOW_QUEUE: &str = "read waiting: admission queue full";
const SLOW_PERMIT: &str = "read waiting: admission permits taken";
mod diagnostics;
pub(super) use diagnostics::Identity;
use diagnostics::{Diagnostics, TrackedRead, Wait};

pub(super) struct ReadAdmission {
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    per_connection: usize,
    /// Waits longer than this log their holders; nothing is rejected.
    slow_wait: Duration,
    diagnostics: Arc<Diagnostics>,
}

pub(super) struct ConnectionReads {
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
}

pub(super) struct WaitingRead {
    engine: Arc<ReadAdmission>,
    connection: Arc<ConnectionReads>,
    /// This connection's queue slot. The engine-wide slot is taken in
    /// [`WaitingRead::acquire`].
    _pending: OwnedSemaphorePermit,
    tracked: Option<TrackedRead>,
}

// Deliberately very high, pending measurement of normal traffic: the
// earlier defaults (32 active, 500 ms wait) rejected the macOS app's own
// startup reads after a cold engine start. These match the overrides that
// restored normal use. Lower them only from measured data, with a generous
// buffer above the observed peak. The env overrides below apply; the
// `_WAIT_MS` ones set the slow-wait log threshold, never a deadline.
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

/// Await `wait`; if it takes longer than `slow`, run `report` once and keep
/// waiting for it.
async fn report_if_slow<F: Future>(wait: F, slow: Duration, report: impl FnOnce()) -> F::Output {
    tokio::pin!(wait);
    match tokio::time::timeout(slow, &mut wait).await {
        Ok(output) => output,
        Err(_) => {
            report();
            wait.await
        }
    }
}

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

    pub(super) fn new(global: usize, per_connection: usize, queue: usize, slow_wait: Duration) -> Self {
        Self {
            active: Arc::new(Semaphore::new(global)),
            pending: Arc::new(Semaphore::new(queue)),
            per_connection,
            slow_wait,
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

    /// Bound each connection's queued reads as well as active handlers. A
    /// full connection queue makes the caller wait for a slot (the reader
    /// loop stops reading that socket meanwhile); a slow wait logs the reads
    /// filling the queue.
    #[cfg(test)]
    pub(super) async fn enqueue(self: &Arc<Self>, connection: &Arc<ConnectionReads>) -> WaitingRead {
        self.enqueue_as(connection, None).await
    }

    /// Like [`Self::enqueue`], naming the request so a slow-queue dump can
    /// identify the waiting read, which is not yet tracked.
    pub(super) async fn enqueue_as(
        self: &Arc<Self>,
        connection: &Arc<ConnectionReads>,
        identity: Option<Identity<'_>>,
    ) -> WaitingRead {
        let slow = |wait: Wait| {
            self.diagnostics
                .report(SLOW_QUEUE, wait, None, identity, Arc::as_ptr(connection) as usize);
        };
        let local = report_if_slow(connection.pending.clone().acquire_owned(), self.slow_wait, || {
            slow(Wait::ConnectionQueue)
        })
        .await
        .expect("connection pending semaphore is never closed");
        WaitingRead {
            engine: self.clone(),
            connection: connection.clone(),
            _pending: local,
            tracked: None,
        }
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

    /// Wait for an engine-wide queue slot, then the connection permit, then
    /// the engine permit. A wait past the lane's slow-wait threshold logs the
    /// holders once.
    pub(super) async fn acquire(self) -> ReadPermits {
        let _engine_pending = report_if_slow(
            self.engine.pending.clone().acquire_owned(),
            self.engine.slow_wait,
            || {
                self.engine.diagnostics.report(
                    SLOW_QUEUE,
                    Wait::EngineQueue,
                    self.tracked.as_ref(),
                    None,
                    self.connection_id(),
                );
            },
        )
        .await
        .expect("engine pending semaphore is never closed");
        let permits = report_if_slow(
            async {
                if let Some(read) = &self.tracked {
                    read.stage("connection permit");
                }
                let local = self
                    .connection
                    .active
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("connection active semaphore is never closed");
                if let Some(read) = &self.tracked {
                    read.stage("engine permit");
                }
                let global = self
                    .engine
                    .active
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("engine active semaphore is never closed");
                (local, global)
            },
            self.engine.slow_wait,
            || {
                self.engine.diagnostics.report(
                    SLOW_PERMIT,
                    Wait::Permit,
                    self.tracked.as_ref(),
                    None,
                    self.connection_id(),
                );
            },
        )
        .await;
        if let Some(read) = &self.tracked {
            read.stage("blocking queue");
        }
        ReadPermits {
            _permits: permits,
            tracked: self.tracked,
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
