//! Bounded by admission's queue/permit counts; entries leave with their guards.
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Most blockers named in one holder dump.
const MAX_BLOCKERS: usize = 16;
/// Holder dumps are rate limited per wait kind so a queue-full flood adds
/// neither lock contention nor log volume of its own, yet cannot suppress the
/// rare deadline dump that names the real blocker.
const DUMP_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub(super) struct Diagnostics {
    next: AtomicU64,
    reads: Mutex<BTreeMap<u64, Entry>>,
    last_dump: Mutex<[Option<Instant>; Wait::KINDS]>,
    suppressed: [AtomicU64; Wait::KINDS],
}

#[derive(Clone, Debug, bon::Builder)]
#[builder(on(String, into))]
struct Entry {
    session: String,
    request: String,
    verb: String,
    stage: &'static str,
    /// Opaque identity of the response view, so separate
    /// views of one verb can be told apart.
    key: u64,
    /// Opaque identity of the connection the read arrived on.
    connection: usize,
    #[builder(default = Instant::now())]
    since: Instant,
}

/// What the rejected read was waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Wait {
    /// The rejected connection's own pending queue was full.
    ConnectionQueue,
    /// The engine-wide pending queue was full.
    EngineQueue,
    /// Connection or engine active permits; the entry's stage says which.
    Permit,
}

impl Wait {
    const KINDS: usize = 3;

    fn index(self) -> usize {
        match self {
            Self::ConnectionQueue => 0,
            Self::EngineQueue => 1,
            Self::Permit => 2,
        }
    }
}

/// Identity of a read rejected before it was tracked (queue full).
#[derive(Clone, Copy, Debug)]
pub(in crate::app) struct Identity<'a> {
    pub(in crate::app) session: &'a str,
    pub(in crate::app) request: &'a str,
    pub(in crate::app) verb: &'a str,
}

#[derive(Debug)]
pub(super) struct TrackedRead {
    diagnostics: Arc<Diagnostics>,
    id: u64,
}

impl Diagnostics {
    pub(super) fn track(
        self: &Arc<Self>,
        session: &str,
        request: &str,
        verb: String,
        key: u64,
        connection: usize,
    ) -> TrackedRead {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.reads.lock().unwrap().insert(
            id,
            Entry::builder()
                .session(session)
                .request(request)
                .verb(verb)
                .stage("admission")
                .key(key)
                .connection(connection)
                .build(),
        );
        TrackedRead {
            diagnostics: self.clone(),
            id,
        }
    }

    /// Logs the reads that were actually holding up the rejected one.
    /// Queue-full rejections happen before the read is tracked, so the
    /// caller passes its `identity` instead; `connection` scopes selection.
    pub(super) fn report(
        &self,
        reason: &str,
        wait: Wait,
        rejected: Option<&TrackedRead>,
        identity: Option<Identity<'_>>,
        connection: usize,
    ) {
        let kind = wait.index();
        {
            let mut last = self.last_dump.lock().unwrap();
            if last[kind].is_some_and(|at| at.elapsed() < DUMP_INTERVAL) {
                self.suppressed[kind].fetch_add(1, Ordering::Relaxed);
                return;
            }
            last[kind] = Some(Instant::now());
        }
        // Copy under the lock; select and format after releasing it.
        let snapshot: Vec<(u64, Entry)> = self
            .reads
            .lock()
            .unwrap()
            .iter()
            .map(|(id, entry)| (*id, entry.clone()))
            .collect();
        let rejected_id = rejected.map(|read| read.id);
        let outstanding = snapshot.len();
        let active = snapshot
            .iter()
            .filter(|(_, entry)| matches!(entry.stage, "blocking queue" | "handler"))
            .count();
        let blockers: Vec<String> = select_blockers(&snapshot, wait, rejected_id, connection)
            .into_iter()
            .map(|(id, entry)| describe(*id, entry))
            .collect();
        let rejected = rejected_id
            .and_then(|id| {
                snapshot
                    .iter()
                    .find(|(entry_id, _)| *entry_id == id)
                    .map(|(id, entry)| describe(*id, entry))
            })
            .or_else(|| {
                identity.map(|who| {
                    format!(
                        "untracked: session={} request={:?} verb={}",
                        who.session, who.request, who.verb
                    )
                })
            });
        let suppressed = self.suppressed[kind].swap(0, Ordering::Relaxed);
        tracing::warn!(
            reason,
            ?wait,
            ?rejected,
            outstanding,
            active,
            suppressed_dumps = suppressed,
            ?blockers,
            "read admission holders"
        );
    }
}

fn describe(id: u64, entry: &Entry) -> String {
    format!(
        "{id}: session={} request={:?} verb={} stage={} key={:x} age_ms={}",
        entry.session,
        entry.request,
        entry.verb,
        entry.stage,
        entry.key,
        entry.since.elapsed().as_millis()
    )
}

/// Oldest-first reads that hold what the rejected read waited for, bounded
/// to `MAX_BLOCKERS` after relevance filtering so unrelated long-running
/// reads cannot crowd the real blocker out of the sample.
fn select_blockers(
    snapshot: &[(u64, Entry)],
    wait: Wait,
    rejected: Option<u64>,
    connection: usize,
) -> Vec<&(u64, Entry)> {
    let rejected_entry = rejected.and_then(|id| snapshot.iter().find(|(entry_id, _)| *entry_id == id));
    // Pending permits are released once a read is admitted, so only reads
    // still waiting for admission fill a pending queue.
    let holds_pending = |stage: &str| matches!(stage, "admission" | "connection permit" | "engine permit");
    let holds_connection_permit = |stage: &str| matches!(stage, "engine permit" | "blocking queue" | "handler");
    let holds_engine_permit = |stage: &str| matches!(stage, "blocking queue" | "handler");
    // Engine permits are shared across connections; everything else is
    // scoped to the rejected read's own connection.
    let engine_wide = wait == Wait::EngineQueue
        || (wait == Wait::Permit && rejected_entry.is_some_and(|(_, r)| r.stage == "engine permit"));
    snapshot
        .iter()
        .filter(|(id, entry)| {
            if Some(*id) == rejected || (entry.connection != connection && !engine_wide) {
                return false;
            }
            match wait {
                Wait::ConnectionQueue | Wait::EngineQueue => holds_pending(entry.stage),
                Wait::Permit if engine_wide => holds_engine_permit(entry.stage),
                Wait::Permit => holds_connection_permit(entry.stage),
            }
        })
        .take(MAX_BLOCKERS)
        .collect()
}

impl TrackedRead {
    pub(super) fn stage(&self, stage: &'static str) {
        if let Some(entry) = self.diagnostics.reads.lock().unwrap().get_mut(&self.id) {
            entry.stage = stage;
        }
    }
}

impl Drop for TrackedRead {
    fn drop(&mut self) {
        self.diagnostics.reads.lock().unwrap().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn diagnostics_follow_permits_until_handler_drop_and_cleanup_cancelled_waiters() {
        let gate = Arc::new(super::super::ReadAdmission::new(
            1,
            1,
            8,
            std::time::Duration::from_millis(10),
        ));
        let connection = gate.connection();
        let waiting = gate
            .enqueue(&connection)
            .unwrap()
            .identify("app", "first", "ListProducts".into(), 1);
        let permits = waiting.acquire().await.unwrap();
        permits.started();
        {
            let reads = gate.diagnostics.reads.lock().unwrap();
            let entry = reads.values().next().unwrap();
            assert_eq!(
                (
                    entry.session.as_str(),
                    entry.request.as_str(),
                    entry.verb.as_str(),
                    entry.stage
                ),
                ("app", "first", "ListProducts", "handler")
            );
        }
        let waiting = gate
            .enqueue(&connection)
            .unwrap()
            .identify("app", "second", "ListHosts".into(), 2);
        assert_eq!(gate.diagnostics.reads.lock().unwrap().len(), 2);
        assert_eq!(waiting.acquire().await.unwrap_err(), super::super::DEADLINE);
        assert_eq!(gate.diagnostics.reads.lock().unwrap().len(), 1);
        let cancelled = gate
            .enqueue(&connection)
            .unwrap()
            .identify("app", "cancelled", "ListHosts".into(), 2);
        drop(cancelled);
        assert_eq!(gate.diagnostics.reads.lock().unwrap().len(), 1);
        drop(permits);
        assert!(gate.diagnostics.reads.lock().unwrap().is_empty());
        assert!(gate.enqueue(&connection).unwrap().acquire().await.is_ok());
    }

    fn entry(session: &str, stage: &'static str, key: u64, connection: usize) -> Entry {
        Entry::builder()
            .session(session)
            .request("r")
            .verb("ListProducts")
            .stage(stage)
            .key(key)
            .connection(connection)
            .build()
    }

    /// Unrelated connections must not hide the connection permit holders.
    #[test]
    fn blockers_name_the_relevant_reads_despite_many_older_unrelated_reads() {
        let mut snapshot: Vec<(u64, Entry)> = (0..20).map(|id| (id, entry("other", "handler", 7, 2))).collect();
        // Same connection, but a different view of the same verb.
        snapshot.push((20, entry("app", "handler", 8, 1)));
        snapshot.push((21, entry("app", "handler", 9, 1)));
        snapshot.push((22, entry("app", "admission", 9, 1)));
        // Connection permit: only the app connection's permit holders.
        snapshot.push((23, entry("app", "connection permit", 10, 1)));
        let blockers = select_blockers(&snapshot, Wait::Permit, Some(23), 1);
        assert_eq!(blockers.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![20, 21]);

        // Engine permit: holders anywhere, bounded after filtering.
        snapshot.push((24, entry("app", "engine permit", 11, 1)));
        let blockers = select_blockers(&snapshot, Wait::Permit, Some(24), 1);
        assert_eq!(blockers.len(), MAX_BLOCKERS);
        assert!(blockers.iter().all(|(_, e)| e.stage == "handler"));

        // Connection queue full: only that connection's still-pending reads,
        // not its older admitted handlers.
        let blockers = select_blockers(&snapshot, Wait::ConnectionQueue, None, 1);
        assert_eq!(blockers.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![22, 23, 24]);
    }

    /// Other connections exhausting the engine queue, behind more than
    /// `MAX_BLOCKERS` older admitted handlers, must still be named.
    #[test]
    fn engine_queue_full_names_pending_holders_on_other_connections() {
        let mut snapshot: Vec<(u64, Entry)> = (0..20).map(|id| (id, entry("old", "handler", 7, 2))).collect();
        snapshot.push((20, entry("other", "connection permit", 8, 2)));
        snapshot.push((21, entry("third", "engine permit", 9, 3)));
        snapshot.push((22, entry("other", "admission", 9, 3)));
        // The rejected connection (1) has nothing pending at all.
        let blockers = select_blockers(&snapshot, Wait::EngineQueue, None, 1);
        assert_eq!(blockers.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![20, 21, 22]);
        let blockers = select_blockers(&snapshot, Wait::ConnectionQueue, None, 1);
        assert!(blockers.is_empty());
    }

    #[tokio::test]
    async fn holder_dumps_are_rate_limited_per_kind_and_count_suppressed_reports() {
        let diagnostics = Arc::new(Diagnostics::default());
        diagnostics.report("first", Wait::ConnectionQueue, None, None, 1);
        diagnostics.report("second", Wait::ConnectionQueue, None, None, 1);
        diagnostics.report("third", Wait::ConnectionQueue, None, None, 1);
        let counts = |d: &Diagnostics| d.suppressed.each_ref().map(|c| c.load(Ordering::Relaxed));
        assert_eq!(counts(&diagnostics), [2, 0, 0]);

        // A deadline report right after a queue-full flood is not suppressed.
        diagnostics.report("permit", Wait::Permit, None, None, 1);
        assert_eq!(counts(&diagnostics), [2, 0, 0]);
        // A second report of the same deadline kind still is, and is attributed to it.
        diagnostics.report("permit again", Wait::Permit, None, None, 1);
        assert_eq!(counts(&diagnostics), [2, 0, 1]);
    }
}
