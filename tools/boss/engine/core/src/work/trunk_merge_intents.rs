//! `trunk_merge_intents` DAO: the standing record that a `trunk_queue`
//! product's PR was submitted to Trunk's merge queue via the merge button.
//!
//! One active row per work item is the poller's tracking anchor and the
//! standing record that this merge was approved by a human click — that
//! record is what authorizes automatic resubmission after an eviction is
//! fixed, so a fix does not require a fresh click (see the design's "Entry
//! state machine"). The partial unique index on `(work_item_id) WHERE status =
//! 'active'` is the dedup gate: [`WorkDb::insert_trunk_merge_intent`] uses
//! `INSERT OR IGNORE`, so a second merge click while an intent is already
//! active returns `Ok(None)` rather than a second row — mirroring the
//! `ci_remediations` dedup idiom.
//!
//! See `trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`
//! §"The merge verb: submit + standing merge intent".

use super::*;

/// One `trunk_merge_intents` row.
///
/// `bon::Builder` is kept here even though nothing constructs one today
/// (this is a DB-read return type, produced only by
/// [`map_trunk_merge_intent`]): checkleft's `rust/giant-structs` check
/// mandates the builder derive on any struct with more than 5 named
/// fields, and this one has more than 5 — mirrors the same rationale on
/// `TrunkPullRequest`/`TrunkQueue` in `boss_trunk_client::models`.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct TrunkMergeIntent {
    pub id: String,
    pub work_item_id: String,
    pub pr_url: String,
    pub pr_number: i64,
    /// `"<owner>/<name>"` — the GitHub repo slug Trunk's API addresses.
    pub repo: String,
    pub target_branch: String,
    /// `active` | `merged` | `cancelled` | `exhausted`. Only `active` is
    /// written by this task; the terminal transitions belong to the
    /// queue poller and eviction/resubmit flows (design tasks 5-7).
    pub status: String,
    /// The most recent Trunk PR state observed by the poller
    /// (`pending`/`testing`/…). `None` until the poller's first probe.
    pub last_trunk_state: Option<String>,
    pub last_trunk_state_at: Option<String>,
    /// How many times `submitPullRequest` has been called for this
    /// intent — `1` after the initial submit, incremented on each
    /// auto-resubmit.
    pub submit_count: i64,
    pub created_at: String,
}

/// Pre-insert payload for [`WorkDb::insert_trunk_merge_intent`].
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct TrunkMergeIntentInsertInput {
    pub work_item_id: String,
    pub pr_url: String,
    pub pr_number: i64,
    pub repo: String,
    pub target_branch: String,
}

/// One `active` intent joined with the two facts about its task the
/// Trunk queue poller needs and the intent row itself doesn't carry:
/// the product the card belongs to (for `work_item_changed` publishes
/// and attention items) and the task's current status (so an intent
/// whose task already reached a terminal status is retired instead of
/// polled forever).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTrunkMergeIntent {
    pub intent: TrunkMergeIntent,
    pub product_id: String,
    pub task_status: String,
}

const TRUNK_MERGE_INTENT_COLUMNS: &str = "id, work_item_id, pr_url, pr_number, repo, target_branch, status, \
     last_trunk_state, last_trunk_state_at, submit_count, created_at";

fn map_trunk_merge_intent(row: &Row<'_>) -> rusqlite::Result<TrunkMergeIntent> {
    Ok(TrunkMergeIntent {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        pr_url: row.get(2)?,
        pr_number: row.get(3)?,
        repo: row.get(4)?,
        target_branch: row.get(5)?,
        status: row.get(6)?,
        last_trunk_state: row.get(7)?,
        last_trunk_state_at: row.get(8)?,
        submit_count: row.get(9)?,
        created_at: row.get(10)?,
    })
}

impl WorkDb {
    /// Insert a fresh `active` `trunk_merge_intents` row for `input`.
    ///
    /// `INSERT OR IGNORE` against the partial unique index on
    /// `(work_item_id) WHERE status = 'active'`: returns `Ok(None)` when an
    /// active intent already exists for this work item — the caller's
    /// duplicate-click no-op path — rather than a second row.
    pub fn insert_trunk_merge_intent(&self, input: TrunkMergeIntentInsertInput) -> Result<Option<TrunkMergeIntent>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let id = next_id("tmi");
        let now = now_string();
        let rows = tx.execute(
            "INSERT OR IGNORE INTO trunk_merge_intents
                (id, work_item_id, pr_url, pr_number, repo, target_branch, status, submit_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 1, ?7)",
            params![
                id,
                input.work_item_id,
                input.pr_url,
                input.pr_number,
                input.repo,
                input.target_branch,
                now,
            ],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let inserted = query_trunk_merge_intent(&tx, &id)?
            .with_context(|| format!("unknown trunk_merge_intent after insert: {id}"))?;
        tx.commit()?;
        Ok(Some(inserted))
    }

    /// The active `trunk_merge_intents` row for `work_item_id`, if any.
    pub fn get_active_trunk_merge_intent(&self, work_item_id: &str) -> Result<Option<TrunkMergeIntent>> {
        let conn = self.connect()?;
        let sql = format!(
            "SELECT {TRUNK_MERGE_INTENT_COLUMNS} FROM trunk_merge_intents \
             WHERE work_item_id = ?1 AND status = 'active'"
        );
        let mut stmt = conn.prepare(&sql)?;
        let row = stmt
            .query_row(params![work_item_id], map_trunk_merge_intent)
            .optional()?;
        Ok(row)
    }

    /// Every `active` intent whose task is still live, oldest first.
    ///
    /// The Trunk queue poller's per-cycle candidate list: it groups these
    /// by `(repo, target_branch)` and issues one `getQueue` per group.
    /// Ordering is `created_at` then `id` so the group's anchor row — the
    /// work item a queue-level attention item attaches to — is stable
    /// across sweeps rather than hash-order dependent.
    ///
    /// Tombstoned tasks are excluded outright (nothing to render or
    /// remediate). Tasks that reached a terminal status are deliberately
    /// *included* so the poller can retire their now-moot intents; it
    /// filters them out before deciding which queues to probe.
    pub fn list_active_trunk_merge_intents(&self) -> Result<Vec<ActiveTrunkMergeIntent>> {
        let conn = self.connect()?;
        // Spelled out rather than derived from `TRUNK_MERGE_INTENT_COLUMNS`:
        // `map_trunk_merge_intent` reads by ordinal, so the join's two extra
        // columns must land at index 11/12 and the intent's own must stay in
        // exactly their declared order.
        let mut stmt = conn.prepare(
            "SELECT i.id, i.work_item_id, i.pr_url, i.pr_number, i.repo, i.target_branch, i.status,
                    i.last_trunk_state, i.last_trunk_state_at, i.submit_count, i.created_at,
                    t.product_id, t.status
             FROM trunk_merge_intents i
             JOIN tasks t ON t.id = i.work_item_id
             WHERE i.status = 'active' AND t.deleted_at IS NULL
             ORDER BY i.created_at ASC, i.id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ActiveTrunkMergeIntent {
                intent: map_trunk_merge_intent(row)?,
                product_id: row.get(11)?,
                task_status: row.get(12)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Record the Trunk PR state the poller just observed for an intent.
    ///
    /// Returns `true` when the stored `last_trunk_state` actually moved,
    /// so the caller can log a transition (and skip event churn) rather
    /// than treating every sweep as news. `last_trunk_state_at` is the
    /// engine's observation time, not Trunk's `stateChangedAt` — it is
    /// what "how long has this entry been sitting in this state?" needs
    /// and it stays on the same clock as every other engine timestamp.
    /// Only ever touches an `active` row.
    pub fn record_trunk_merge_intent_state(&self, id: &str, state: &str) -> Result<bool> {
        let conn = self.connect()?;
        let changed = conn.execute(
            "UPDATE trunk_merge_intents
             SET last_trunk_state = ?2, last_trunk_state_at = ?3
             WHERE id = ?1
               AND status = 'active'
               AND (last_trunk_state IS NULL OR last_trunk_state <> ?2)",
            params![id, state, now_string()],
        )?;
        Ok(changed > 0)
    }

    /// Retire an intent into a terminal `status` (`merged` | `cancelled` |
    /// `exhausted`), freeing the `(work_item_id) WHERE status = 'active'`
    /// dedup slot for a future merge click.
    ///
    /// Guarded on `status = 'active'` so two sweeps racing the same
    /// terminal observation retire it exactly once — the returned `bool`
    /// is that "I am the one who retired it" signal, which the caller uses
    /// to fire the Review snap-back and attention item at most once.
    ///
    /// Deliberately leaves `last_trunk_state` alone: the intent's status
    /// and the queue's own state are different facts (an intent can be
    /// retired because its task went terminal, with no Trunk observation
    /// at all). Callers with an observed state record it via
    /// [`WorkDb::record_trunk_merge_intent_state`] *before* retiring —
    /// that method is itself `status = 'active'`-guarded.
    pub fn retire_trunk_merge_intent(&self, id: &str, status: &str) -> Result<bool> {
        let conn = self.connect()?;
        let rows = conn.execute(
            "UPDATE trunk_merge_intents SET status = ?2 WHERE id = ?1 AND status = 'active'",
            params![id, status],
        )?;
        Ok(rows > 0)
    }

    /// Delete a `trunk_merge_intents` row outright — used when a Trunk
    /// `submitPullRequest` call fails right after the intent was recorded,
    /// so "no intent row survives" a failed submission (design §"The merge
    /// verb"). Idempotent: deleting an unknown id is a no-op.
    pub fn delete_trunk_merge_intent(&self, id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute("DELETE FROM trunk_merge_intents WHERE id = ?1", params![id])?;
        Ok(())
    }
}

fn query_trunk_merge_intent(conn: &Connection, id: &str) -> Result<Option<TrunkMergeIntent>> {
    let sql = format!("SELECT {TRUNK_MERGE_INTENT_COLUMNS} FROM trunk_merge_intents WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt.query_row(params![id], map_trunk_merge_intent).optional()?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

    fn input(work_item_id: &str) -> TrunkMergeIntentInsertInput {
        TrunkMergeIntentInsertInput::builder()
            .work_item_id(work_item_id)
            .pr_url("https://github.com/brianduff/flunge/pull/978")
            .pr_number(978)
            .repo("brianduff/flunge")
            .target_branch("main")
            .build()
    }

    #[test]
    fn insert_creates_an_active_row() {
        let db = test_db();
        let intent = db
            .insert_trunk_merge_intent(input("task_1"))
            .expect("insert succeeds")
            .expect("first insert is not a duplicate");
        assert_eq!(intent.work_item_id, "task_1");
        assert_eq!(intent.status, "active");
        assert_eq!(intent.submit_count, 1);
        assert_eq!(intent.pr_number, 978);
        assert_eq!(intent.repo, "brianduff/flunge");
        assert_eq!(intent.target_branch, "main");
        assert!(intent.last_trunk_state.is_none());
    }

    #[test]
    fn second_insert_for_the_same_work_item_is_ignored_while_active() {
        let db = test_db();
        db.insert_trunk_merge_intent(input("task_1")).unwrap().unwrap();
        let second = db.insert_trunk_merge_intent(input("task_1")).unwrap();
        assert!(second.is_none(), "duplicate active intent must not insert a second row");
    }

    #[test]
    fn insert_for_a_different_work_item_is_unaffected_by_an_existing_active_intent() {
        let db = test_db();
        db.insert_trunk_merge_intent(input("task_1")).unwrap().unwrap();
        let other = db
            .insert_trunk_merge_intent(input("task_2"))
            .unwrap()
            .expect("a different work item is not blocked by task_1's active intent");
        assert_eq!(other.work_item_id, "task_2");
    }

    #[test]
    fn get_active_returns_none_when_no_intent_exists() {
        let db = test_db();
        assert!(db.get_active_trunk_merge_intent("task_1").unwrap().is_none());
    }

    #[test]
    fn get_active_finds_the_inserted_row() {
        let db = test_db();
        let inserted = db.insert_trunk_merge_intent(input("task_1")).unwrap().unwrap();
        let found = db.get_active_trunk_merge_intent("task_1").unwrap().unwrap();
        assert_eq!(found.id, inserted.id);
    }

    #[test]
    fn delete_removes_the_row_and_frees_the_work_item_for_a_fresh_intent() {
        let db = test_db();
        let intent = db.insert_trunk_merge_intent(input("task_1")).unwrap().unwrap();
        db.delete_trunk_merge_intent(&intent.id).unwrap();
        assert!(db.get_active_trunk_merge_intent("task_1").unwrap().is_none());
        // Freed: a fresh insert for the same work item now succeeds.
        let fresh = db.insert_trunk_merge_intent(input("task_1")).unwrap();
        assert!(fresh.is_some());
    }

    #[test]
    fn delete_of_an_unknown_id_is_a_no_op() {
        let db = test_db();
        db.delete_trunk_merge_intent("tmi_does_not_exist").unwrap();
    }

    // ── poller-facing reads/writes ────────────────────────────────────

    /// A real task row so `list_active_trunk_merge_intents`'s join to
    /// `tasks` (for `product_id` / `status`) has something to match.
    fn seed_task(db: &WorkDb, name: &str) -> (String, String) {
        let product = crate::test_support::create_test_product_named(db, &format!("Product-{name}"));
        let task = crate::test_support::create_test_chore_manual(db, product.id.clone(), name);
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        (product.id, task.id)
    }

    fn input_for(work_item_id: &str) -> TrunkMergeIntentInsertInput {
        TrunkMergeIntentInsertInput::builder()
            .work_item_id(work_item_id)
            .pr_url("https://github.com/brianduff/flunge/pull/978")
            .pr_number(978)
            .repo("brianduff/flunge")
            .target_branch("main")
            .build()
    }

    #[test]
    fn list_active_joins_product_and_task_status() {
        let db = test_db();
        let (product_id, task_id) = seed_task(&db, "queued-task");
        db.insert_trunk_merge_intent(input_for(&task_id)).unwrap().unwrap();

        let listed = db.list_active_trunk_merge_intents().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].intent.work_item_id, task_id);
        assert_eq!(listed[0].intent.status, "active");
        assert_eq!(listed[0].product_id, product_id);
        assert_eq!(listed[0].task_status, "in_review");
    }

    #[test]
    fn list_active_excludes_retired_intents_and_tombstoned_tasks() {
        let db = test_db();
        let (_, retired_task) = seed_task(&db, "retired");
        let retired = db.insert_trunk_merge_intent(input_for(&retired_task)).unwrap().unwrap();
        db.retire_trunk_merge_intent(&retired.id, "cancelled").unwrap();

        let (_, deleted_task) = seed_task(&db, "deleted");
        db.insert_trunk_merge_intent(input_for(&deleted_task)).unwrap().unwrap();
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET deleted_at = '1' WHERE id = ?1", params![deleted_task])
            .unwrap();

        assert!(db.list_active_trunk_merge_intents().unwrap().is_empty());
    }

    #[test]
    fn record_state_reports_only_real_transitions() {
        let db = test_db();
        let (_, task_id) = seed_task(&db, "state");
        let intent = db.insert_trunk_merge_intent(input_for(&task_id)).unwrap().unwrap();

        assert!(db.record_trunk_merge_intent_state(&intent.id, "pending").unwrap());
        // Same state again is not news — no write, no transition log.
        assert!(!db.record_trunk_merge_intent_state(&intent.id, "pending").unwrap());
        assert!(db.record_trunk_merge_intent_state(&intent.id, "testing").unwrap());

        let stored = db.get_active_trunk_merge_intent(&task_id).unwrap().unwrap();
        assert_eq!(stored.last_trunk_state.as_deref(), Some("testing"));
        assert!(stored.last_trunk_state_at.is_some());
    }

    #[test]
    fn record_state_ignores_an_already_retired_intent() {
        let db = test_db();
        let (_, task_id) = seed_task(&db, "retired-state");
        let intent = db.insert_trunk_merge_intent(input_for(&task_id)).unwrap().unwrap();
        db.retire_trunk_merge_intent(&intent.id, "cancelled").unwrap();

        assert!(!db.record_trunk_merge_intent_state(&intent.id, "testing").unwrap());
    }

    #[test]
    fn retire_is_single_shot_and_frees_the_work_item() {
        let db = test_db();
        let (_, task_id) = seed_task(&db, "retire");
        let intent = db.insert_trunk_merge_intent(input_for(&task_id)).unwrap().unwrap();
        db.record_trunk_merge_intent_state(&intent.id, "cancelled").unwrap();

        assert!(db.retire_trunk_merge_intent(&intent.id, "cancelled").unwrap());
        // A second sweep observing the same terminal state must not
        // re-fire the snap-back / attention item.
        assert!(!db.retire_trunk_merge_intent(&intent.id, "cancelled").unwrap());

        assert!(db.get_active_trunk_merge_intent(&task_id).unwrap().is_none());
        // Retiring preserves the last observed Trunk state rather than
        // overwriting it with the intent's own status vocabulary.
        let row: (String, Option<String>) = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT status, last_trunk_state FROM trunk_merge_intents WHERE id = ?1",
                params![intent.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("cancelled".to_owned(), Some("cancelled".to_owned())));
        // The dedup slot is free: a fresh merge click can enqueue again.
        assert!(db.insert_trunk_merge_intent(input_for(&task_id)).unwrap().is_some());
    }
}
