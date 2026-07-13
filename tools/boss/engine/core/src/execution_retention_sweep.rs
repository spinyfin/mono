//! Periodic retention sweep for terminal `work_executions` rows.
//!
//! Applies [`crate::work::ExecutionRetentionPolicy`] (see
//! `crate::work::execution_retention` for the full policy rationale) on a
//! recurring cadence so the stock of terminal executions — dominated in
//! practice by `redundant_spawn` pre-spawn aborts — never grows unbounded.
//!
//! Because [`crate::sweep_loop::spawn_sweep_loop`] fires immediately on
//! spawn, the first pass after this ships also performs the one-time
//! cleanup of whatever backlog had already accumulated (e.g. the
//! T2168/T2215 `redundant_spawn` storm) — there is no separate migration
//! step. Every subsequent pass just keeps the stock bounded going forward.

use std::sync::Arc;
use std::time::Duration;

use crate::work::{ExecutionRetentionPolicy, WorkDb};

/// How often the retention sweep runs. Deliberately much slower than the
/// worker-liveness sweeps (dead-pid, stale-worker, …): pruning terminal
/// rows is not time-sensitive, and the delete is a no-op in steady state
/// once the backlog is clean.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Counts from one sweep pass; logged whenever any row was pruned.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExecutionRetentionSweepOutcome {
    pub deleted: u64,
}

impl crate::sweep_loop::SweepOutcome for ExecutionRetentionSweepOutcome {
    fn has_activity(&self) -> bool {
        self.deleted > 0
    }

    fn log(&self) {
        tracing::info!(
            deleted = self.deleted,
            "execution-retention sweep: pruned terminal work_executions rows past the retention bound",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(work_db: Arc<WorkDb>, interval: Duration) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        async move { run_one_pass(work_db.as_ref()).await }
    })
}

/// Run a single retention pass with the default policy. Returns a summary;
/// callers may log it.
pub async fn run_one_pass(work_db: &WorkDb) -> ExecutionRetentionSweepOutcome {
    let now_epoch = crate::epoch_time::now_epoch_secs();

    match work_db.prune_terminal_executions(ExecutionRetentionPolicy::default(), now_epoch, false) {
        Ok(outcome) => ExecutionRetentionSweepOutcome {
            deleted: outcome.deleted,
        },
        Err(err) => {
            tracing::warn!(?err, "execution-retention sweep: prune failed; skipping this pass");
            ExecutionRetentionSweepOutcome::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::work::CreateChoreInput;

    #[tokio::test]
    async fn sweep_prunes_old_abandoned_backlog_on_first_pass() {
        let (_dir, db) = open_db();
        let product_id = create_test_product_with_repo(&db, "test-product", Some("https://github.com/test/repo")).id;
        let work_item_id = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("redundant-spawn-storm")
                    .build(),
            )
            .unwrap()
            .id;

        let now_epoch = crate::epoch_time::now_epoch_secs();
        let very_old = now_epoch - 400 * 24 * 60 * 60;
        for _ in 0..10 {
            let execution = db
                .request_execution(
                    boss_protocol::RequestExecutionInput::builder()
                        .work_item_id(work_item_id.clone())
                        .build(),
                )
                .unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = 'abandoned', created_at = ?2 WHERE id = ?1",
                rusqlite::params![execution.id, very_old.to_string()],
            )
            .unwrap();
        }

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert!(outcome.deleted > 0, "first pass must clean up the pre-existing backlog");

        let remaining = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            remaining.len() < 10,
            "backlog beyond the keep floor must be pruned, got {} remaining",
            remaining.len()
        );
    }

    #[tokio::test]
    async fn sweep_is_a_noop_once_clean() {
        let (_dir, db) = open_db();
        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.deleted, 0);
    }
}
