//! Once-per-state logging for the reconciler's decline-to-mint gates.
//!
//! `reconcile_work_item_execution` runs on every pass for every pending row,
//! so a row that is held for good (a PR awaiting review, a deliberate park)
//! would repeat the same line each time. [`DeclineLog`] remembers the last
//! `(governing execution, gate)` state logged per work item: the line is
//! emitted when that state is entered and stays quiet until it changes. A
//! successful mint clears the memory, so a later decline logs again.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Bound on remembered work items; past it the memory resets, which at worst
/// repeats one line per held row.
const CAP: usize = 4096;

#[derive(Default)]
pub(crate) struct DeclineLog {
    last: Mutex<HashMap<String, (Option<String>, &'static str)>>,
}

impl DeclineLog {
    /// The process-wide log the reconciler uses.
    pub(crate) fn global() -> &'static DeclineLog {
        static GLOBAL: OnceLock<DeclineLog> = OnceLock::new();
        GLOBAL.get_or_init(DeclineLog::default)
    }

    /// Record that `gate` declined to mint for `work_item_id` under the
    /// governing execution `governing`. Returns `true` when this is a new
    /// state, i.e. the caller should log it.
    pub(crate) fn entered(&self, work_item_id: &str, governing: Option<&str>, gate: &'static str) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.len() >= CAP {
            last.clear();
        }
        let state = (governing.map(str::to_owned), gate);
        if last.get(work_item_id) == Some(&state) {
            return false;
        }
        last.insert(work_item_id.to_owned(), state);
        true
    }

    /// The row minted an execution, so the next decline is a new state.
    pub(crate) fn cleared(&self, work_item_id: &str) {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).remove(work_item_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_decline_logs_once_until_the_state_changes() {
        let log = DeclineLog::default();
        assert!(log.entered("task_1", Some("exec_1"), "deliberately_parked"));
        assert!(!log.entered("task_1", Some("exec_1"), "deliberately_parked"));
        assert!(!log.entered("task_1", Some("exec_1"), "deliberately_parked"));
        // A different gate, or a different governing execution, is a new state.
        assert!(log.entered("task_1", Some("exec_1"), "pr_awaits_review"));
        assert!(log.entered("task_1", Some("exec_2"), "pr_awaits_review"));
        // Another work item is independent.
        assert!(log.entered("task_2", Some("exec_2"), "pr_awaits_review"));
    }

    #[test]
    fn minting_resets_the_memory() {
        let log = DeclineLog::default();
        assert!(log.entered("task_1", None, "pr_awaits_review"));
        log.cleared("task_1");
        assert!(log.entered("task_1", None, "pr_awaits_review"));
    }

    #[test]
    fn unchanged_reconcile_does_not_reenter_the_decline() {
        use crate::test_support::{create_test_chore_manual, create_test_product};
        use crate::work::*;
        let logs = crate::test_support::log_capture::install();
        let db = WorkDb::open_in_memory().unwrap();
        let product = create_test_product(&db);
        let task = create_test_chore_manual(&db, product.id, "review hold");
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET autostart = 1, pr_url = 'https://github.com/example/repo/pull/1' WHERE id = ?1",
            [&task.id],
        )
        .unwrap();
        for _ in 0..2 {
            let mut result = ExecutionReconcileResult::default();
            reconcile_work_item_execution(
                &conn,
                &mut result,
                &task.id,
                ExecutionKind::ChoreImplementation,
                ExecutionStatus::Ready,
            )
            .unwrap();
            assert!(!DeclineLog::global().entered(&task.id, None, DISPATCH_HOLD_PR_AWAITS_REVIEW));
            assert!(query_latest_execution_for_work_item(&conn, &task.id).unwrap().is_none());
        }
        let text = String::from_utf8_lossy(&logs.lock()).into_owned();
        assert_eq!(
            text.lines()
                .filter(|line| line.contains(&task.id) && line.contains("skipping execution mint"))
                .count(),
            1
        );
    }
}
