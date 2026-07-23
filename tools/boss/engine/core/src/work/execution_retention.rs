//! Retention/compaction for terminal `work_executions` rows.
//!
//! ## The problem
//!
//! Every pre-spawn abort (the `redundant_spawn` guard in
//! `coordinator::schedule_execution`, and any other early dispatch
//! failure) mints a `work_executions` row and immediately marks it
//! `abandoned` — it never gets cleaned up. In one incident a
//! `redundant_spawn` storm produced 2,087 such rows in a single night
//! before the underlying inflow bug (T2168/T2215) was fixed. Because nothing
//! ever prunes terminal executions, this stock only grows, dragging down
//! every query that scans `work_executions` (including, transitively, the
//! `automation_runs` history rendered by the Automations pane — see
//! `work::automations::list_automation_runs`).
//!
//! ## The policy
//!
//! [`WorkDb::prune_terminal_executions`] deletes rows that are BOTH:
//! - in a prunable terminal status (`abandoned`, `failed`, `orphaned`,
//!   `cancelled` — deliberately excludes `completed`, which is the
//!   canonical record of shipped work and comparatively rare next to the
//!   retry/abort noise this exists to bound), AND
//! - older than [`ExecutionRetentionPolicy::max_age_secs`], AND
//! - outside the most recent [`ExecutionRetentionPolicy::keep_per_work_item`]
//!   prunable executions for their work item.
//!
//! The last condition is the diagnostics floor: incident forensics (T2217,
//! T2233) leaned heavily on recent failure history, so a work item that
//! fails repeatedly always keeps its most recent failures on hand even
//! once they cross the age bound — only the long tail beyond the floor is
//! ever removed. A later successful (`completed`) execution of the same
//! work item does not itself delete anything; it is superseded implicitly
//! once its sibling failures age out or fall outside the keep-window,
//! which is simpler to reason about than an explicit "superseded by a
//! later success" join and produces the same practical outcome.
//!
//! `work_runs.execution_id`, `work_attention_items.execution_id`, and
//! `worker_proposals.execution_id` are all `ON DELETE CASCADE`, so a pruned
//! execution's runs, any (typically already-resolved) attention items, and
//! any worker proposals pointing at it go with it.
//! `automation_runs.triage_execution_id` has no FK — that row's
//! `outcome`/`detail` are denormalized at write time, so a pruned triage
//! execution just leaves that column dangling; the automation's run
//! history stays intact and readable.
//!
//! This runs on a recurring schedule ([`crate::execution_retention_sweep`],
//! which also performs the one-time backlog cleanup on first boot after
//! this ships — the sweep fires immediately on spawn) and is available as
//! an on-demand operator verb (`bossctl executions prune`) for manual
//! cleanup between sweeps.

use super::*;

/// `work_executions.status` values eligible for pruning. `completed` is
/// intentionally absent — see the module doc.
const PRUNABLE_STATUSES_SQL: &str = "'abandoned', 'failed', 'orphaned', 'cancelled'";

/// Default age bound: prune eligible executions older than 14 days.
/// Comfortably outlives the window an operator would plausibly need
/// ("what happened to this task last week?") while still bounding the
/// hot query path against a storm like T2168/T2215.
pub const DEFAULT_RETENTION_MAX_AGE_SECS: i64 = 14 * 24 * 60 * 60;

/// Default diagnostics floor: always keep at least this many of the most
/// recent eligible executions per work item, regardless of age.
pub const DEFAULT_RETENTION_KEEP_PER_WORK_ITEM: u32 = 5;

/// Retention bound for [`WorkDb::prune_terminal_executions`]. See the
/// module doc for the exact semantics of the two fields together.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionRetentionPolicy {
    pub max_age_secs: i64,
    pub keep_per_work_item: u32,
}

impl Default for ExecutionRetentionPolicy {
    fn default() -> Self {
        Self {
            max_age_secs: DEFAULT_RETENTION_MAX_AGE_SECS,
            keep_per_work_item: DEFAULT_RETENTION_KEEP_PER_WORK_ITEM,
        }
    }
}

/// Outcome of one [`WorkDb::prune_terminal_executions`] call.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionPruneOutcome {
    /// Rows deleted — or, when `dry_run` was requested, rows that WOULD
    /// have been deleted.
    pub deleted: u64,
}

impl WorkDb {
    /// Prune (or, with `dry_run`, just count) terminal `work_executions`
    /// rows past `policy`'s bound. `now_epoch` is UTC epoch seconds —
    /// callers pass a fixed value (rather than reading the clock inside)
    /// so pruning is deterministic in tests.
    pub fn prune_terminal_executions(
        &self,
        policy: ExecutionRetentionPolicy,
        now_epoch: i64,
        dry_run: bool,
    ) -> Result<ExecutionPruneOutcome> {
        let conn = self.connect()?;
        prune_terminal_executions_on(&conn, policy, now_epoch, dry_run)
    }
}

fn prune_terminal_executions_on(
    conn: &Connection,
    policy: ExecutionRetentionPolicy,
    now_epoch: i64,
    dry_run: bool,
) -> Result<ExecutionPruneOutcome> {
    let cutoff = now_epoch.saturating_sub(policy.max_age_secs);
    // The keep-window is computed over prunable-status rows only (a
    // `completed` execution never occupies a `keep_per_work_item` slot),
    // ranked newest-first per work item so `rn <= keep_per_work_item`
    // picks the most recent ones.
    let candidates_sql = format!(
        "SELECT id FROM work_executions
          WHERE status IN ({PRUNABLE_STATUSES_SQL})
            AND CAST(created_at AS INTEGER) < ?1
            AND id NOT IN (
                SELECT id FROM (
                    SELECT id, ROW_NUMBER() OVER (
                        PARTITION BY work_item_id
                        ORDER BY CAST(created_at AS INTEGER) DESC, id DESC
                    ) AS rn
                    FROM work_executions
                    WHERE status IN ({PRUNABLE_STATUSES_SQL})
                )
                WHERE rn <= ?2
            )"
    );

    if dry_run {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM ({candidates_sql})"),
            params![cutoff, policy.keep_per_work_item],
            |row| row.get(0),
        )?;
        return Ok(ExecutionPruneOutcome { deleted: count as u64 });
    }

    let deleted = conn.execute(
        &format!("DELETE FROM work_executions WHERE id IN ({candidates_sql})"),
        params![cutoff, policy.keep_per_work_item],
    )?;
    Ok(ExecutionPruneOutcome {
        deleted: deleted as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::work::CreateChoreInput;
    use boss_protocol::RequestExecutionInput;

    fn open_db() -> WorkDb {
        WorkDb::open_in_memory().unwrap()
    }

    fn create_chore(db: &WorkDb, product_id: &str, name: &str) -> String {
        db.create_chore(
            CreateChoreInput::builder()
                .product_id(product_id.to_owned())
                .name(name.to_owned())
                .build(),
        )
        .unwrap()
        .id
    }

    /// Insert an execution for `work_item_id`, force its status and
    /// `created_at` (epoch seconds), and return its id.
    fn insert_execution(db: &WorkDb, work_item_id: &str, status: &str, created_at_epoch: i64) -> String {
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = ?2, created_at = ?3 WHERE id = ?1",
            rusqlite::params![execution.id, status, created_at_epoch.to_string()],
        )
        .unwrap();
        execution.id
    }

    const DAY: i64 = 24 * 60 * 60;

    #[test]
    fn prunes_old_abandoned_rows_beyond_the_keep_floor() {
        let db = open_db();
        let product_id = create_test_product_with_repo(&db, "p", Some("https://github.com/test/repo")).id;
        let work_item_id = create_chore(&db, &product_id, "c1");
        let now = 1_800_000_000i64;

        // 4 old abandoned rows, all well past the age bound. With a
        // keep-floor of 1, only the newest of the 4 survives.
        for i in 0..4 {
            insert_execution(&db, &work_item_id, "abandoned", now - 20 * DAY - i);
        }

        let policy = ExecutionRetentionPolicy {
            max_age_secs: DEFAULT_RETENTION_MAX_AGE_SECS,
            keep_per_work_item: 1,
        };
        let outcome = db.prune_terminal_executions(policy, now, false).unwrap();
        assert_eq!(outcome.deleted, 3, "3 of the 4 old rows fall outside the keep-1 floor");

        let remaining = db.list_executions(Some(&work_item_id)).unwrap();
        assert_eq!(remaining.len(), 1, "only the newest kept-floor row remains");
    }

    #[test]
    fn prunes_executions_with_worker_proposal_rows_via_cascade() {
        let db = open_db();
        let product_id = create_test_product_with_repo(&db, "p", Some("https://github.com/test/repo")).id;
        let work_item_id = create_chore(&db, &product_id, "c1");
        let now = 1_800_000_000i64;

        let execution_id = insert_execution(&db, &work_item_id, "abandoned", now - 20 * DAY);

        // A worker_proposals row referencing a prunable execution must not
        // block the bulk `DELETE FROM work_executions` with a FOREIGN KEY
        // constraint violation — the FK is ON DELETE CASCADE, so the
        // proposal row is expected to go with its execution.
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO worker_proposals
                 (id, execution_id, work_item_id, kind, payload_json, idempotency_key, created_at)
             VALUES ('prop_1', ?1, ?2, 'pr_created', '{}', 'idem_1', ?3)",
            rusqlite::params![execution_id, work_item_id, now.to_string()],
        )
        .unwrap();
        drop(conn);

        let policy = ExecutionRetentionPolicy {
            max_age_secs: DEFAULT_RETENTION_MAX_AGE_SECS,
            keep_per_work_item: 0,
        };
        let outcome = db.prune_terminal_executions(policy, now, false).unwrap();
        assert_eq!(
            outcome.deleted, 1,
            "the sole old execution is pruned despite the referencing proposal row"
        );

        let conn = db.connect().unwrap();
        let remaining_proposals: i64 = conn
            .query_row("SELECT COUNT(*) FROM worker_proposals", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            remaining_proposals, 0,
            "the cascade removes the proposal row along with its execution"
        );
    }

    #[test]
    fn keep_floor_protects_old_rows_even_past_the_age_bound() {
        let db = open_db();
        let product_id = create_test_product_with_repo(&db, "p", Some("https://github.com/test/repo")).id;
        let work_item_id = create_chore(&db, &product_id, "c1");
        let now = 1_800_000_000i64;

        // 3 old abandoned rows, past the age bound. A recent one is also
        // present, but the recent row consumes the keep-1 slot (it is the
        // newest prunable row for this work item overall) — the floor is
        // "most recent N", not "N per age bucket".
        for i in 0..3 {
            insert_execution(&db, &work_item_id, "abandoned", now - 20 * DAY - i);
        }
        insert_execution(&db, &work_item_id, "abandoned", now - DAY);

        let policy = ExecutionRetentionPolicy {
            max_age_secs: DEFAULT_RETENTION_MAX_AGE_SECS,
            keep_per_work_item: 1,
        };
        let outcome = db.prune_terminal_executions(policy, now, false).unwrap();
        assert_eq!(
            outcome.deleted, 3,
            "all 3 old rows are pruned; the recent row already holds the keep-1 slot"
        );

        let remaining = db.list_executions(Some(&work_item_id)).unwrap();
        assert_eq!(remaining.len(), 1, "only the recent row remains");
    }

    #[test]
    fn never_prunes_completed_executions() {
        let db = open_db();
        let product_id = create_test_product_with_repo(&db, "p", Some("https://github.com/test/repo")).id;
        let work_item_id = create_chore(&db, &product_id, "c1");
        let now = 1_800_000_000i64;
        insert_execution(&db, &work_item_id, "completed", now - 400 * DAY);

        let outcome = db
            .prune_terminal_executions(ExecutionRetentionPolicy::default(), now, false)
            .unwrap();
        assert_eq!(outcome.deleted, 0);
        assert_eq!(db.list_executions(Some(&work_item_id)).unwrap().len(), 1);
    }

    #[test]
    fn keep_floor_is_per_work_item() {
        let db = open_db();
        let product_id = create_test_product_with_repo(&db, "p", Some("https://github.com/test/repo")).id;
        let item_a = create_chore(&db, &product_id, "a");
        let item_b = create_chore(&db, &product_id, "b");
        let now = 1_800_000_000i64;

        for i in 0..3 {
            insert_execution(&db, &item_a, "abandoned", now - 20 * DAY - i);
            insert_execution(&db, &item_b, "abandoned", now - 20 * DAY - i);
        }

        let policy = ExecutionRetentionPolicy {
            max_age_secs: DEFAULT_RETENTION_MAX_AGE_SECS,
            keep_per_work_item: 1,
        };
        let outcome = db.prune_terminal_executions(policy, now, false).unwrap();
        assert_eq!(outcome.deleted, 4, "1 kept per work item, so 2 of 3 pruned for each");
        assert_eq!(db.list_executions(Some(&item_a)).unwrap().len(), 1);
        assert_eq!(db.list_executions(Some(&item_b)).unwrap().len(), 1);
    }

    #[test]
    fn dry_run_counts_without_deleting() {
        let db = open_db();
        let product_id = create_test_product_with_repo(&db, "p", Some("https://github.com/test/repo")).id;
        let work_item_id = create_chore(&db, &product_id, "c1");
        let now = 1_800_000_000i64;
        insert_execution(&db, &work_item_id, "failed", now - 20 * DAY);

        let policy = ExecutionRetentionPolicy {
            max_age_secs: DEFAULT_RETENTION_MAX_AGE_SECS,
            keep_per_work_item: 0,
        };
        let outcome = db.prune_terminal_executions(policy, now, true).unwrap();
        assert_eq!(outcome.deleted, 1);
        assert_eq!(
            db.list_executions(Some(&work_item_id)).unwrap().len(),
            1,
            "dry run must not actually delete"
        );
    }

    #[test]
    fn recent_rows_within_age_bound_survive_regardless_of_floor() {
        let db = open_db();
        let product_id = create_test_product_with_repo(&db, "p", Some("https://github.com/test/repo")).id;
        let work_item_id = create_chore(&db, &product_id, "c1");
        let now = 1_800_000_000i64;
        for i in 0..5 {
            insert_execution(&db, &work_item_id, "abandoned", now - i * 60);
        }

        let policy = ExecutionRetentionPolicy {
            max_age_secs: DEFAULT_RETENTION_MAX_AGE_SECS,
            keep_per_work_item: 0,
        };
        let outcome = db.prune_terminal_executions(policy, now, false).unwrap();
        assert_eq!(outcome.deleted, 0, "all rows are inside the age bound");
    }
}
