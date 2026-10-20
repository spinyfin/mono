//! Per-session single-flight for identical admitted reads.
//!
//! A burst of identical refreshes (the app re-reading one view several
//! times, or pipelining the same list on one socket) costs one handler run:
//! the first request leads, later identical ones follow and are answered
//! from the leader's responses under their own request ids.
//!
//! A follower may only join a leader whose snapshot is not yet taken, so its
//! answer is never older than its own arrival — a read issued after a write
//! on the same session can only share a read whose snapshot is taken after
//! that write committed. That is the case while the leader is queued for
//! admission, and while its handler is blocked on a SQLite lock
//! ([`ReadWait`]): a write transaction holds the lock from the moment the
//! leader's wait ends until it commits, so no write the follower could have
//! waited on lands between the follower's arrival and the leader's snapshot.
//! Once the leader's handler is past its lock wait, the next identical read
//! leads a new batch that runs after it.
//!
//! Followers keep their admission queue permits until they are answered, so
//! the reader loop's reply-headroom accounting (every outstanding read holds
//! a pending or active permit) stays exact. A follower is also registered
//! with the session's per-view freshness tracking at the moment it joins, so
//! its answer is delivered under the same ordering rule as the leader's: if a
//! later batch for the same view has already replied, the follower receives
//! that newer snapshot rather than overwriting it with an older one.
//!
//! Scope is one session: authorization and worker-tier sanitisation are
//! per-session, so whole responses are not shared across sessions. The
//! database load behind `GetWorkTree`, which is a pure function of the
//! database, is shared engine-wide instead (see `work_tree_flight`).
use super::read_admission::WaitingRead;
use super::session_queue::SessionSink;
use crate::work::contention::ReadWait;
use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequest};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Engine-wide counters, held on `ServerState` so a test can observe how
/// many handler runs a burst cost.
#[derive(Debug, Default)]
pub(in crate::app) struct Stats {
    leaders: AtomicU64,
    followers: AtomicU64,
}

#[cfg(test)]
impl Stats {
    pub(in crate::app) fn leaders(&self) -> u64 {
        self.leaders.load(Ordering::Relaxed)
    }

    pub(in crate::app) fn followers(&self) -> u64 {
        self.followers.load(Ordering::Relaxed)
    }
}

/// Whether identical requests may share one handler run. `GetWorkTree`
/// encodes its response against the session's delta cache, which the first
/// response mutates, so a replayed copy would be wrong for the second; it
/// shares only its database load, per waiter (see `work_tree_flight`).
pub(in crate::app) fn coalescable(request: &FrontendRequest) -> bool {
    !matches!(request, FrontendRequest::GetWorkTree { .. })
}

/// Owned by one session's reader loop: the open batch per exact request.
#[derive(Default)]
pub(in crate::app) struct ReadCoalescer {
    batches: HashMap<String, Arc<Batch>>,
}

pub(in crate::app) enum Role {
    /// This request runs the handler, keeping its own admission wait; the
    /// batch collects its followers.
    Lead { batch: Arc<Batch>, waiting: WaitingRead },
    /// This request is answered when the batch's leader finishes.
    Follow,
}

impl ReadCoalescer {
    /// Join `request` to the open batch for its exact serialisation, or
    /// open a new one if there is none or the existing leader's snapshot is
    /// taken. A follower parks `pending` (its queue permits) until it is
    /// answered; `register` runs under the batch's lock once the follower has
    /// joined, so the answer cannot be sent before it has run.
    pub(in crate::app) fn join(
        &mut self,
        request: &FrontendRequest,
        request_id: &str,
        pending: WaitingRead,
        stats: &Stats,
        register: impl FnOnce(),
    ) -> Role {
        // A batch nobody else references any more has finished and been
        // untapped; only the map keeps it alive.
        self.batches.retain(|_, batch| Arc::strong_count(batch) > 1);
        let key = serde_json::to_string(request).expect("serializable request");
        let follower = Follower {
            request_id: request_id.to_owned(),
            _pending: pending,
        };
        let follower = match self.batches.get(&key) {
            Some(batch) => match batch.try_follow(follower, register) {
                Ok(()) => {
                    stats.followers.fetch_add(1, Ordering::Relaxed);
                    return Role::Follow;
                }
                Err(follower) => follower,
            },
            None => follower,
        };
        let batch = Arc::new(Batch {
            leader_request_id: request_id.to_owned(),
            state: Mutex::new(State::default()),
            responses: Mutex::new(Vec::new()),
            wait: Arc::default(),
        });
        self.batches.insert(key, batch.clone());
        stats.leaders.fetch_add(1, Ordering::Relaxed);
        Role::Lead {
            batch,
            waiting: follower._pending,
        }
    }
}

struct Follower {
    request_id: String,
    _pending: WaitingRead,
}

#[derive(Default)]
struct State {
    /// The leader's handler has begun. From then on the batch is joinable
    /// only while that handler is blocked on a SQLite lock.
    started: bool,
    /// The leader has returned (or failed): nothing may join.
    closed: bool,
    followers: Vec<Follower>,
}

/// One leader and the identical reads answered from its responses.
pub(in crate::app) struct Batch {
    leader_request_id: String,
    state: Mutex<State>,
    /// The leader's response envelopes, captured by the session sink's tap.
    responses: Mutex<Vec<FrontendEventEnvelope>>,
    /// Set by the DB layer while the leader's handler waits for a SQLite lock.
    wait: Arc<ReadWait>,
}

impl Batch {
    pub(in crate::app) fn leader_request_id(&self) -> &str {
        &self.leader_request_id
    }

    /// The flag the DB layer sets while the leader's handler is blocked on a
    /// SQLite lock, installed as [`crate::work::contention::READ_WAIT`] around
    /// the leader's dispatch.
    pub(in crate::app) fn wait(&self) -> Arc<ReadWait> {
        self.wait.clone()
    }

    fn try_follow(&self, follower: Follower, register: impl FnOnce()) -> Result<(), Follower> {
        let mut state = self.state.lock().expect("read batch lock poisoned");
        if state.closed || (state.started && !self.wait.is_waiting()) {
            return Err(follower);
        }
        state.followers.push(follower);
        register();
        Ok(())
    }

    /// Called by the sink tap for every envelope carrying the leader's id.
    pub(in crate::app) fn record(&self, envelope: &FrontendEventEnvelope) {
        self.responses
            .lock()
            .expect("read batch lock poisoned")
            .push(envelope.clone());
    }

    /// The leader is about to run its handler: from here on, identical
    /// reads open a new batch instead of joining this one, unless the handler
    /// is blocked on a SQLite lock. Returns the number of followers that will
    /// share this run so far.
    pub(in crate::app) fn start(&self) -> usize {
        let mut state = self.state.lock().expect("read batch lock poisoned");
        state.started = true;
        state.followers.len()
    }

    /// The leader's handler returned: answer every follower with the
    /// leader's responses under the follower's own request id, then
    /// release the followers' queue permits.
    pub(in crate::app) fn finish(&self, sink: &SessionSink) {
        let followers = self.take_followers();
        if followers.is_empty() {
            return;
        }
        let responses = std::mem::take(&mut *self.responses.lock().expect("read batch lock poisoned"));
        if responses.is_empty() {
            // A read handler that answered nothing is an engine bug, not
            // contention; the followers must still not hang.
            tracing::error!(
                leader = %self.leader_request_id,
                followers = followers.len(),
                "coalesced read produced no response for its followers"
            );
            self.answer(sink, followers, |_| FrontendEvent::Error {
                message: "coalesced read produced no response".into(),
            });
            return;
        }
        tracing::debug!(
            leader = %self.leader_request_id,
            followers = followers.len(),
            responses = responses.len(),
            "coalesced read answered its followers"
        );
        for follower in followers {
            for response in &responses {
                sink.enqueue(FrontendEventEnvelope {
                    request_id: Some(follower.request_id.clone()),
                    revision: response.revision,
                    payload: response.payload.clone(),
                });
            }
        }
    }

    /// The leader's handler failed outright (it panicked); its followers
    /// get the same failure the leader was sent.
    pub(in crate::app) fn fail(&self, sink: &SessionSink, message: &str) {
        let followers = self.take_followers();
        self.answer(sink, followers, |_| FrontendEvent::Error {
            message: message.to_owned(),
        });
    }

    fn take_followers(&self) -> Vec<Follower> {
        let mut state = self.state.lock().expect("read batch lock poisoned");
        state.started = true;
        state.closed = true;
        std::mem::take(&mut state.followers)
    }

    fn answer(&self, sink: &SessionSink, followers: Vec<Follower>, payload: impl Fn(&Follower) -> FrontendEvent) {
        for follower in followers {
            sink.enqueue(FrontendEventEnvelope::response(
                follower.request_id.clone(),
                payload(&follower),
            ));
        }
    }
}

#[cfg(test)]
#[path = "read_coalesce_tests.rs"]
mod tests;
