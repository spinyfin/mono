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

/// Outbound entries the reader loop keeps free for non-read replies and pushes.
const MIN_READER_HEADROOM: usize = 55;

/// Largest per-connection active limit such that both lanes' outstanding
/// replies still leave [`MIN_READER_HEADROOM`] outbound entries.
const MAX_PER_CONNECTION: usize =
    (super::session_queue::MAX_SESSION_QUEUE - MIN_READER_HEADROOM - 1) / (2 * (PENDING_PER_ACTIVE + 1));
const _: () = assert!(
    2 * MAX_PER_CONNECTION * (PENDING_PER_ACTIVE + 1) + 1 + MIN_READER_HEADROOM
        <= super::session_queue::MAX_SESSION_QUEUE
);

/// Outbound-queue depth the reader loop waits for before reading another
/// request: every reply both lanes may have outstanding, plus one.
pub(super) fn reply_headroom(bulk: &ReadAdmission, live: &ReadAdmission) -> usize {
    bulk.outstanding_per_connection()
        .saturating_add(live.outstanding_per_connection())
        .saturating_add(1)
}

fn connection_setting(name: &str, default: usize) -> usize {
    let value = setting(name, default);
    if value > MAX_PER_CONNECTION {
        tracing::warn!(
            name,
            value,
            MAX_PER_CONNECTION,
            "clamping read admission setting to preserve reply headroom"
        );
    }
    value.min(MAX_PER_CONNECTION)
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

impl WaitingRead {
    pub(super) async fn acquire_ordered(
        mut self,
        order: ordering::Turn,
    ) -> Result<
        (
            (OwnedSemaphorePermit, OwnedSemaphorePermit),
            tokio::sync::OwnedMutexGuard<()>,
        ),
        &'static str,
    > {
        // Waiting behind an identical in-flight read is not admission
        // pressure, so it has its own deadline; the admission deadline starts
        // once this read's turn comes up.
        let turn = tokio::time::timeout(ORDER_WAIT, order).await.map_err(|_| BUSY)?;
        self.deadline = tokio::time::Instant::now() + self.engine.wait;
        let permits = self.acquire().await?;
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
