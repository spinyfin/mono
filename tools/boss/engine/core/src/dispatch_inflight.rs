//! In-memory registry of dispatches that have been handed off to a
//! concurrent task but have not yet started their run.
//!
//! # Why this exists
//!
//! Every dispatch guard that asks "is another execution already working this
//! PR/chain?" ([`crate::coordinator::ExecutionCoordinator::resolve_chain_hold`]
//! and the double-spawn guard in `schedule_execution`) answers from the DB:
//! `work_executions.status IN ('running', 'waiting_human')`. An execution is
//! CAS'd to `claimed` at pickup so the product reconciler cannot rewrite it,
//! but `claimed` is not live — `start_execution_run_on_host` still commits
//! `running` as the last step of `schedule_execution`, after `cube repo ensure`,
//! the workspace lease, `cube workspace goto` and `cube change create`.
//!
//! While `drain_ready_queue` awaited `schedule_execution` inline, that was
//! sound by construction: item A was `running` in the DB before item B's guard
//! ever ran, so the DB was a complete picture of "who is dispatching". Handing
//! the slow tail off to a concurrent task destroys that property. Two ready
//! rows on the same chain would each look up a DB that shows no live sibling —
//! because neither has started yet — and both would dispatch, putting two
//! writers on the one shared jj backing store cube gives every same-PR
//! workspace. That is precisely the T1577/T1815 two-writer corruption the chain
//! guard exists to prevent, and `schedule_execution`'s post-lease TOCTOU
//! assertion does not catch it: both tasks can pass that check before either
//! goes live.
//!
//! This registry closes the window. A dispatch is registered *synchronously*,
//! in the drain loop's serial decision section, before the loop moves to the
//! next ready row — so the very next guard sees it. The guards then union DB
//! liveness with the reservations held here, restoring the invariant the serial
//! loop used to provide for free.
//!
//! Reservations are unique per execution and claimed atomically. Work items
//! remain exclusive except for admissible members of one persisted review batch
//! ([`InflightDispatches::try_reserve`]) — see that method for why both
//! properties are load-bearing rather than incidental.
//!
//! # Lifetime
//!
//! A reservation is held from "the drain loop decided to dispatch this row"
//! until `schedule_execution` returns, by which point either
//! `start_execution_run_on_host` has committed (the DB now shows `running`, so
//! the registry can safely forget it — there is no gap) or dispatch failed and
//! the row is back to `ready`/`failed` (from `claimed`) with its claim and lease released.
//!
//! [`DispatchReservation`] de-registers on `Drop` rather than at an explicit
//! call site. `schedule_execution` has well over a dozen early-return paths
//! (redundant spawn, chain sibling went live, lease failure, occupied
//! workspace, goto failure, change-create failure, run-start failure, …); a
//! reservation leaked on any one of them would wedge every future dispatch on
//! that chain for the lifetime of the engine process. `Drop` is the only way to
//! cover all of them, including a panic in the handed-off task.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use boss_protocol::WorkExecution;

use crate::work::WorkDb;

/// Shared reservation table, keyed by execution id.
type Table = Arc<Mutex<HashMap<String, WorkExecution>>>;

/// Registry of in-flight (handed-off but not yet started) dispatches.
///
/// Cheap to clone — clones share one table.
#[derive(Clone, Default)]
pub struct InflightDispatches {
    table: Table,
}

impl InflightDispatches {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the right to dispatch `execution`, or `None` if its work item is
    /// already being dispatched by an incompatible execution.
    ///
    /// Check-and-insert stays atomic under one lock, including the persisted
    /// batch check. The same execution can never own two guards: otherwise
    /// dropping either would remove the other's reservation. Ordinary work
    /// items remain exclusive, including duplicate ready rows from the orphan
    /// sweep. Only the DB double-spawn guard's same-batch exception is shared.
    /// A failed membership lookup blocks dispatch rather than admitting a
    /// possible second writer.
    ///
    /// Callers MUST hold the returned guard for the whole dispatch — move it
    /// into the handed-off task, or keep it alive across the inline
    /// `schedule_execution` await. Dropping it early reopens the two-writer
    /// window this registry exists to close.
    #[must_use = "dropping the reservation immediately de-registers the dispatch, \
                  reopening the two-writer window it exists to close"]
    pub fn try_reserve(&self, execution: &WorkExecution, db: &WorkDb) -> Option<DispatchReservation> {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        if Self::conflicts(&table, execution, db) {
            return None;
        }
        table.insert(execution.id.clone(), execution.clone());
        Some(DispatchReservation {
            table: Arc::clone(&self.table),
            execution_id: execution.id.clone(),
        })
    }

    /// `true` when no dispatch is currently in flight.
    ///
    /// Lets callers skip the chain-membership query on the overwhelmingly
    /// common path where nothing has been handed off.
    pub fn is_empty(&self) -> bool {
        self.table.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Pre-filter for drain rows. Rechecked atomically by `try_reserve`.
    /// Rejects a repeated execution and incompatible same-work-item rows,
    /// while allowing persisted same-batch members through to dispatch.
    pub fn blocks_execution(&self, execution: &WorkExecution, db: &WorkDb) -> bool {
        let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        Self::conflicts(&table, execution, db)
    }

    fn conflicts(table: &HashMap<String, WorkExecution>, execution: &WorkExecution, db: &WorkDb) -> bool {
        // The batch helper recognizes roles, not reservation ownership. In
        // particular, a leaf compared with itself is an admissible DB pair.
        if table.contains_key(&execution.id) {
            return true;
        }
        table
            .values()
            .filter(|e| e.work_item_id == execution.work_item_id)
            .any(
                |other| match db.are_admissible_same_review_batch_pair(&execution.id, &other.id) {
                    Ok(admissible) => !admissible,
                    Err(err) => {
                        tracing::warn!(execution_id = %execution.id, other_execution_id = %other.id,
                        ?err, "cannot verify review batch compatibility; holding dispatch");
                        true
                    }
                },
            )
    }

    /// Whether any dispatch for this work item is in flight, including review
    /// members. Used for work-item-level admission, not individual drain rows.
    pub fn is_work_item_dispatching(&self, work_item_id: &str) -> bool {
        self.table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|e| e.work_item_id == work_item_id)
    }

    /// In-flight dispatches on work items in `member_ids`, excluding
    /// `exclude_execution_id` (the caller itself) and excluding
    /// `exclude_work_item_id`.
    ///
    /// The work-item exclusion mirrors
    /// [`crate::work::WorkDb::live_executions_elsewhere_in_chain`], which skips
    /// the caller's own work item because same-work-item duplicates are the
    /// double-spawn guard's job, not the chain guard's. Here they are
    /// additionally impossible: [`Self::try_reserve`] excludes incompatible
    /// same-work-item dispatches.
    pub fn chain_siblings(
        &self,
        member_ids: &[String],
        exclude_work_item_id: &str,
        exclude_execution_id: &str,
    ) -> Vec<WorkExecution> {
        let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        member_ids
            .iter()
            .filter(|member_id| member_id.as_str() != exclude_work_item_id)
            .flat_map(|member_id| {
                table
                    .values()
                    .filter(move |e| &e.work_item_id == member_id)
                    .filter(|e| e.id != exclude_execution_id)
                    .cloned()
            })
            .collect()
    }
}

/// RAII handle de-registering its dispatch from [`InflightDispatches`] on
/// `Drop`. See the module docs for why de-registration is `Drop`-driven.
pub struct DispatchReservation {
    table: Table,
    execution_id: String,
}

impl Drop for DispatchReservation {
    fn drop(&mut self) {
        self.table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::ExecutionKind;

    fn execution(id: &str, work_item_id: &str) -> WorkExecution {
        WorkExecution::builder()
            .id(id)
            .work_item_id(work_item_id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(boss_protocol::ExecutionStatus::Ready)
            .repo_remote_url("https://github.com/example/repo")
            .created_at("2026-07-15T00:00:00Z")
            .build()
    }

    #[test]
    fn reservation_is_visible_to_the_chain_guard_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let inflight = InflightDispatches::new();
        let members = vec!["T1".to_string(), "T2".to_string()];

        assert!(inflight.is_empty());

        let _reservation = inflight
            .try_reserve(&execution("exec_a", "T1"), &db)
            .expect("first reservation");

        assert!(!inflight.is_empty());
        let siblings = inflight.chain_siblings(&members, "T2", "exec_b");
        assert_eq!(
            siblings.len(),
            1,
            "a dispatch in flight on T1 must block chain sibling T2"
        );
        assert_eq!(siblings[0].id, "exec_a");
    }

    #[test]
    fn reservation_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let inflight = InflightDispatches::new();
        let members = vec!["T1".to_string(), "T2".to_string()];

        {
            let _reservation = inflight
                .try_reserve(&execution("exec_a", "T1"), &db)
                .expect("first reservation");
            assert!(!inflight.chain_siblings(&members, "T2", "exec_b").is_empty());
        }

        assert!(inflight.is_empty(), "dropping the guard must de-register the dispatch");
        assert!(
            inflight.chain_siblings(&members, "T2", "exec_b").is_empty(),
            "a completed dispatch must stop blocking its chain",
        );
    }

    #[test]
    fn chain_siblings_excludes_the_caller_itself() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let inflight = InflightDispatches::new();
        let members = vec!["T1".to_string(), "T2".to_string()];

        let _reservation = inflight
            .try_reserve(&execution("exec_a", "T1"), &db)
            .expect("first reservation");

        // `exec_a` resolving its own chain hold must not see its own
        // reservation — every guard inside `schedule_execution` re-checks
        // while the reservation is held, and self-blocking would refuse
        // every handed-off dispatch at the post-lease assertion.
        assert!(
            inflight.chain_siblings(&members, "T1", "exec_a").is_empty(),
            "an execution must not block on its own reservation",
        );
    }

    #[test]
    fn chain_siblings_ignores_work_items_outside_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let inflight = InflightDispatches::new();

        let _reservation = inflight
            .try_reserve(&execution("exec_a", "T9"), &db)
            .expect("first reservation");

        assert!(
            inflight
                .chain_siblings(&["T1".to_string(), "T2".to_string()], "T2", "exec_b")
                .is_empty(),
            "a dispatch on an unrelated chain must not block",
        );
    }

    /// Two duplicate `ready` rows for one work item (the orphan-sweep race)
    /// must not both be dispatchable. If both were handed off they would each
    /// see the other mid-dispatch and both abandon, leaving the work item with
    /// no live execution at all.
    #[test]
    fn a_second_row_for_the_same_work_item_cannot_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let inflight = InflightDispatches::new();

        let _first = inflight
            .try_reserve(&execution("exec_a", "T1"), &db)
            .expect("first reservation");

        assert!(
            inflight.try_reserve(&execution("exec_b", "T1"), &db).is_none(),
            "a duplicate row for a work item already dispatching must not get a reservation",
        );
        assert!(inflight.is_work_item_dispatching("T1"));
        assert!(!inflight.is_work_item_dispatching("T2"));
    }

    /// `try_reserve` is the atomic claim. A `contains`-then-`reserve` pair
    /// would let two callers both insert under one key, so the first `Drop`
    /// would de-register a dispatch that is still running.
    #[test]
    fn a_rejected_reservation_does_not_disturb_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let inflight = InflightDispatches::new();
        let members = vec!["T1".to_string(), "T2".to_string()];

        let _first = inflight
            .try_reserve(&execution("exec_a", "T1"), &db)
            .expect("first reservation");
        // A racing `force_dispatch` for the same row is refused...
        assert!(inflight.try_reserve(&execution("exec_a", "T1"), &db).is_none());
        // ...and, crucially, refusing it left the live reservation intact
        // rather than overwriting it with a second, separately-dropped entry.
        let siblings = inflight.chain_siblings(&members, "T2", "exec_b");
        assert_eq!(siblings.len(), 1);
        assert_eq!(siblings[0].id, "exec_a");
    }

    /// The work item becomes reservable again once the dispatch completes —
    /// otherwise a failed dispatch would wedge its work item forever.
    #[test]
    fn a_work_item_is_reservable_again_after_its_dispatch_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let inflight = InflightDispatches::new();

        drop(
            inflight
                .try_reserve(&execution("exec_a", "T1"), &db)
                .expect("first reservation"),
        );

        assert!(
            inflight.try_reserve(&execution("exec_b", "T1"), &db).is_some(),
            "a retry of a finished dispatch must be able to reserve its work item",
        );
    }

    #[test]
    fn reservations_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let inflight = InflightDispatches::new();
        let members = vec!["T1".to_string(), "T2".to_string()];

        let first = inflight
            .try_reserve(&execution("exec_a", "T1"), &db)
            .expect("first reservation");
        let _second = inflight
            .try_reserve(&execution("exec_b", "T2"), &db)
            .expect("distinct work item");

        drop(first);

        // Dropping T1's reservation must leave T2's intact.
        let siblings = inflight.chain_siblings(&members, "T1", "exec_c");
        assert_eq!(siblings.len(), 1);
        assert_eq!(siblings[0].id, "exec_b");
    }
}
