//! Publish-after-commit seam for the engine event bus.
//!
//! ## Why this exists
//!
//! A state-write path typically looks like:
//!
//! ```text
//! let tx = conn.transaction()?;
//! // ... writes ...
//! tx.commit()?;
//! ```
//!
//! If a producer called [`boss_event_bus::EventBus::publish`] before
//! `tx.commit()`, a rolled-back transaction (an early `?` return, a
//! constraint violation, a panic unwinding through the `Transaction`'s
//! `Drop`) would still have announced a state transition that never
//! actually happened. [`PendingEvents`] + [`commit_and_publish`] close that
//! window: events are staged in memory during the transaction and only
//! handed to the bus once the commit itself has returned `Ok`. If the
//! commit never happens — or fails — the staged events are simply dropped
//! along with `PendingEvents` going out of scope.
//!
//! **Publish-after-commit is the default for every producer.** The one
//! documented exception is the dependency-unblock cascade
//! (`cascade_dependents_after_prereq_status_change`, see
//! [`crate::dep_unblock_sweep`]), which legitimately acts *inside* the
//! transaction it's part of; that path should additionally stage a
//! post-commit `DependencyPrereqsSatisfied` event here for other bus
//! subscribers rather than publish in-txn itself.
//!
//! Execution-terminal transitions in `work/` stage through
//! `stage_execution_terminal` and commit via [`commit_and_publish`], as does
//! `ProjectImplDrained`, staged by
//! `work::design_postmortem::stage_project_impl_drained_on_terminal_transition`
//! from the same status-write paths in `work/updates.rs`, `work/pr_flow.rs`,
//! and `work/exec_tail.rs`. Host-disable transitions publish directly (no
//! enclosing transaction).

use boss_event_bus::{Event, EventBus};

/// Events staged by a state-write path for publication once its enclosing
/// DB transaction commits. Construct one per transaction, [`push`](Self::push)
/// every event the write produces, then hand it to [`commit_and_publish`]
/// alongside the `Transaction` — never call [`EventBus::publish`] directly
/// from inside a transaction.
#[derive(Debug, Default)]
pub struct PendingEvents(Vec<Event>);

impl PendingEvents {
    /// An empty set of staged events.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage `event` to publish once the transaction commits.
    pub fn push(&mut self, event: Event) {
        self.0.push(event);
    }

    /// True if nothing has been staged yet.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Consume `self`, yielding every staged event. For the rare
    /// non-transactional producer that has no `Transaction` to hand
    /// [`commit_and_publish`] — its single `Connection::execute` write
    /// already auto-committed, so there's nothing left to gate the
    /// publish on; the caller publishes each event immediately instead.
    pub fn into_events(self) -> Vec<Event> {
        self.0
    }
}

/// Commit `tx`, and only if the commit succeeds, publish every event staged
/// in `pending` onto `bus`. Returns the commit's own `Result` unchanged —
/// on `Err`, `pending` is dropped without publishing anything, exactly as a
/// rolled-back transaction should never have announced a transition that
/// didn't happen.
pub fn commit_and_publish(
    tx: rusqlite::Transaction<'_>,
    pending: PendingEvents,
    bus: &EventBus,
) -> rusqlite::Result<()> {
    tx.commit()?;
    for event in pending.0 {
        bus.publish(event);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use boss_event_bus::TopicFilter;
    use rusqlite::Connection;

    use super::*;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
            .expect("create table");
        conn
    }

    #[tokio::test]
    async fn publishes_after_successful_commit() {
        let mut conn = open_conn();
        let bus = EventBus::new();
        let mut sub = bus.subscribe(TopicFilter::all());

        let tx = conn.transaction().expect("open txn");
        tx.execute("INSERT INTO t (id) VALUES (1)", []).expect("insert");
        let mut pending = PendingEvents::new();
        pending.push(Event::DispatchReady);
        commit_and_publish(tx, pending, &bus).expect("commit");

        let event = sub.recv().await.expect("event delivered after commit");
        assert_eq!(event, Event::DispatchReady);
    }

    #[tokio::test]
    async fn drops_events_when_commit_never_runs() {
        // Simulate a state-write path that stages events but bails out
        // (an early `?` return, or a panic unwinding) before ever calling
        // `commit_and_publish`. The transaction rolls back on drop and the
        // staged events are discarded with `pending` — nothing is published.
        let mut conn = open_conn();
        let bus = EventBus::new();
        let mut sub = bus.subscribe(TopicFilter::all());

        {
            let tx = conn.transaction().expect("open txn");
            tx.execute("INSERT INTO t (id) VALUES (2)", []).expect("insert");
            let mut pending = PendingEvents::new();
            pending.push(Event::DispatchReady);
            assert!(!pending.is_empty());
            // `tx` and `pending` both drop here without `commit_and_publish`.
        }

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .expect("count rows");
        assert_eq!(rows, 0, "rolled-back insert must not be visible");

        bus.publish(Event::HostDisabled {
            host_id: "unrelated".to_string(),
        });
        let event = sub.recv().await.expect("unrelated event still delivered");
        assert_eq!(
            event,
            Event::HostDisabled {
                host_id: "unrelated".to_string()
            },
            "only the unrelated post-rollback publish should arrive; the staged DispatchReady must never appear"
        );
    }

    #[tokio::test]
    async fn drops_events_when_commit_fails() {
        // Force `tx.commit()` itself to return `Err` (a deferred foreign-key
        // violation, checked at COMMIT rather than at the offending INSERT)
        // and assert `commit_and_publish` propagates the error without
        // publishing the staged event.
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute("PRAGMA foreign_keys = ON", [])
            .expect("enable foreign keys");
        conn.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", [])
            .expect("create parent table");
        conn.execute(
            "CREATE TABLE child (
                id INTEGER PRIMARY KEY,
                parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED
            )",
            [],
        )
        .expect("create child table");

        let bus = EventBus::new();
        let mut sub = bus.subscribe(TopicFilter::all());

        let tx = conn.transaction().expect("open txn");
        tx.execute("INSERT INTO child (id, parent_id) VALUES (1, 999)", [])
            .expect("deferred FK insert succeeds inside the txn");
        let mut pending = PendingEvents::new();
        pending.push(Event::DispatchReady);

        let result = commit_and_publish(tx, pending, &bus);
        assert!(
            result.is_err(),
            "commit must fail: deferred FK violation is checked at COMMIT"
        );

        bus.publish(Event::HostDisabled {
            host_id: "unrelated".to_string(),
        });
        let event = sub.recv().await.expect("unrelated event still delivered");
        assert_eq!(
            event,
            Event::HostDisabled {
                host_id: "unrelated".to_string()
            },
            "only the unrelated post-failure publish should arrive; the staged DispatchReady must never appear"
        );
    }
}
