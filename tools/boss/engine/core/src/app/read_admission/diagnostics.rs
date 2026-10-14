//! Bounded by admission's queue/permit counts; entries leave with their guards.
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

#[derive(Debug, Default)]
pub(super) struct Diagnostics {
    next: AtomicU64,
    reads: Mutex<BTreeMap<u64, Entry>>,
}

#[derive(Debug)]
struct Entry {
    session: String,
    request: String,
    verb: String,
    stage: &'static str,
    since: Instant,
}

#[derive(Debug)]
pub(super) struct TrackedRead {
    diagnostics: Arc<Diagnostics>,
    id: u64,
}

impl Diagnostics {
    pub(super) fn track(self: &Arc<Self>, session: &str, request: &str, verb: String) -> TrackedRead {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.reads.lock().unwrap().insert(
            id,
            Entry {
                session: session.into(),
                request: request.into(),
                verb,
                stage: "ordering",
                since: Instant::now(),
            },
        );
        TrackedRead {
            diagnostics: self.clone(),
            id,
        }
    }

    pub(super) fn report(&self, reason: &str) {
        let reads = self.reads.lock().unwrap();
        // Keep warning volume bounded even if a runaway client fills the queue.
        let oldest: Vec<_> = reads
            .iter()
            .take(16)
            .map(|(id, entry)| {
                format!(
                    "{id}: session={} request={:?} verb={} stage={} age_ms={}",
                    entry.session,
                    entry.request,
                    entry.verb,
                    entry.stage,
                    entry.since.elapsed().as_millis()
                )
            })
            .collect();
        let active = reads
            .values()
            .filter(|entry| matches!(entry.stage, "blocking queue" | "handler"))
            .count();
        let outstanding = reads.len();
        drop(reads);
        tracing::warn!(reason, outstanding, active, ?oldest, "read admission holders");
    }
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
            .identify("app", "first", "ListProducts".into());
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
            .identify("app", "second", "ListHosts".into());
        assert_eq!(gate.diagnostics.reads.lock().unwrap().len(), 2);
        assert_eq!(waiting.acquire().await.unwrap_err(), super::super::DEADLINE);
        assert_eq!(gate.diagnostics.reads.lock().unwrap().len(), 1);
        let cancelled = gate
            .enqueue(&connection)
            .unwrap()
            .identify("app", "cancelled", "ListHosts".into());
        drop(cancelled);
        assert_eq!(gate.diagnostics.reads.lock().unwrap().len(), 1);
        drop(permits);
        assert!(gate.diagnostics.reads.lock().unwrap().is_empty());
        assert!(gate.enqueue(&connection).unwrap().acquire().await.is_ok());
    }
}
