//! Bounded frontend read admission. Internal work and mutations never enter
//! this lane. Acquire the connection permit before the engine permit so a
//! single pipelined connection cannot occupy the engine's whole wait queue.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Queued (not yet active) reads allowed per active permit on one connection.
const PENDING_PER_ACTIVE: usize = 4;

/// Longest a read waits for an identical earlier read on the connection.
/// Exceeds the 45 s on-demand fetch budget of the slowest network-bound read.
const ORDER_WAIT: Duration = Duration::from_secs(60);

pub(super) const BUSY: &str = "engine busy, retry: read admission limit exceeded";

pub(super) struct ReadAdmission {
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    per_connection: usize,
    wait: Duration,
}

pub(super) struct ConnectionReads {
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
}

pub(super) struct WaitingRead {
    engine: Arc<ReadAdmission>,
    connection: Arc<ConnectionReads>,
    _engine_pending: OwnedSemaphorePermit,
    _connection_pending: OwnedSemaphorePermit,
    deadline: tokio::time::Instant,
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
    pub(super) fn enqueue(self: &Arc<Self>, connection: &Arc<ConnectionReads>) -> Result<WaitingRead, &'static str> {
        let local = connection.pending.clone().try_acquire_owned().map_err(|_| BUSY)?;
        let global = self.pending.clone().try_acquire_owned().map_err(|_| BUSY)?;
        Ok(WaitingRead {
            engine: self.clone(),
            connection: connection.clone(),
            _engine_pending: global,
            _connection_pending: local,
            deadline: tokio::time::Instant::now() + self.wait,
        })
    }
}

/// Why an accepted read was rejected while waiting. Both reach the client as
/// the same busy reply; the distinction only selects which warning is logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rejection {
    /// Expired behind an earlier identical read (fixed `ORDER_WAIT`).
    OrderingWait,
    /// Expired waiting for admission permits (`BOSS_RPC_*_READ_WAIT_MS`).
    AdmissionWait,
}

impl Rejection {
    pub(super) fn message(self) -> &'static str {
        BUSY
    }
}

impl WaitingRead {
    pub(super) async fn acquire_ordered(
        mut self,
        order: ordering::Turn,
    ) -> Result<
        (
            (OwnedSemaphorePermit, OwnedSemaphorePermit),
            tokio::sync::OwnedMutexGuard<()>,
        ),
        Rejection,
    > {
        // Waiting behind an identical in-flight read is not admission
        // pressure, so it has its own deadline; the admission deadline starts
        // once this read's turn comes up.
        let turn = tokio::time::timeout(ORDER_WAIT, order)
            .await
            .map_err(|_| Rejection::OrderingWait)?;
        self.deadline = tokio::time::Instant::now() + self.engine.wait;
        let permits = self.acquire().await.map_err(|_| Rejection::AdmissionWait)?;
        Ok((permits, turn))
    }

    pub(super) async fn acquire(self) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), &'static str> {
        tokio::time::timeout_at(self.deadline, async {
            let local = self.connection.active.clone().acquire_owned().await.map_err(|_| BUSY)?;
            let global = self.engine.active.clone().acquire_owned().await.map_err(|_| BUSY)?;
            Ok((local, global))
        })
        .await
        .map_err(|_| BUSY)?
    }
}

mod lane;
pub(super) use lane::{ReadLane, read_lane};

mod ordering;
pub(super) use ordering::ReadOrdering;

#[cfg(test)]
mod burst_tests;
#[cfg(test)]
mod tests;
