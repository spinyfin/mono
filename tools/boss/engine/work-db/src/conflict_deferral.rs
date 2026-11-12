//! Ledger side of just-in-time merge-conflict resolution.
//!
//! A conflict on a PR nobody has asked to merge is *recorded* here and not
//! resolved. The record is an ordinary `conflict_resolutions` row in status
//! [`CONFLICT_STATUS_DEFERRED`]: it is never `pending` or `running`, so every
//! reader that treats those two statuses as "a fix is in flight" (the
//! conflict watcher, CI watch, the progression view's clause 2, the
//! background-work list) keeps reporting that nothing is being done about the
//! conflict, which is true.
//!
//! A deferral ends exactly once, by [`WorkDb::end_conflict_deferral`], which
//! moves the row to [`CONFLICT_STATUS_DEFERRAL_ENDED`] and stamps why in
//! `deferral_end_reason`. Ending it does not resolve anything: the reasons
//! for which [`ConflictDeferralEnd::hands_to_resolver`] is true leave the
//! conflict owed to the normal resolution path, which then inserts its own
//! attempt row. So one deferred conflict that is later resolved reads in the
//! ledger as a `deferred` → `superseded` record followed by the attempt that
//! resolved it.
//!
//! The decision of *when* to defer and when to stop lives in the engine
//! (`conflict_deferral` in `boss-engine`); this module only keeps the rows
//! consistent. See `tools/boss/docs/conflict-resolution-deferral.md`.

use super::*;
use rusqlite::TransactionBehavior;

pub(super) const MIGRATION_ID: &str = "20261111_1900_conflict_resolution_deferral";

/// Status of a ledger row that records a conflict whose resolution is being
/// held back. Deliberately not one of the in-flight statuses.
pub const CONFLICT_STATUS_DEFERRED: &str = "deferred";

/// Status a deferral row moves to when the deferral is over. The row was
/// overtaken by something else (a resolution attempt, the PR going clean,
/// the PR ending), which is what `superseded` already means in this ledger.
pub const CONFLICT_STATUS_DEFERRAL_ENDED: &str = "superseded";

/// `conflict_resolutions.deferral_arm` for a row in the treatment arm.
pub const CONFLICT_DEFERRAL_ARM_DEFERRED: &str = "deferred";

/// Why a deferral ended. Stored in `conflict_resolutions.deferral_end_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictDeferralEnd {
    /// A merge was requested for the PR, so it is ready apart from the
    /// conflict.
    MergeRequested,
    /// A human asked for this conflict to be resolved now.
    HumanRequest,
    /// Deferral was switched off, or the PR left the treatment arm.
    DeferralDisabled,
    /// A resolution attempt for the PR started through another path.
    ResolutionStarted,
    /// GitHub reported the PR mergeable again without a resolution run.
    ConflictCleared,
    /// The PR merged.
    PrMerged,
    /// The PR was closed without merging.
    PrClosed,
}

impl ConflictDeferralEnd {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MergeRequested => "merge_requested",
            Self::HumanRequest => "human_request",
            Self::DeferralDisabled => "deferral_disabled",
            Self::ResolutionStarted => "resolution_started",
            Self::ConflictCleared => "conflict_cleared",
            Self::PrMerged => "pr_merged",
            Self::PrClosed => "pr_closed",
        }
    }

    /// `true` when the conflict still exists and is now owed a resolution.
    /// Such a conflict must not be deferred a second time.
    pub fn hands_to_resolver(self) -> bool {
        matches!(
            self,
            Self::MergeRequested | Self::HumanRequest | Self::DeferralDisabled | Self::ResolutionStarted
        )
    }

    fn parse(value: &str) -> Option<Self> {
        [
            Self::MergeRequested,
            Self::HumanRequest,
            Self::DeferralDisabled,
            Self::ResolutionStarted,
            Self::ConflictCleared,
            Self::PrMerged,
            Self::PrClosed,
        ]
        .into_iter()
        .find(|end| end.as_str() == value)
    }
}

/// What [`WorkDb::record_conflict_deferral`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictDeferralRecord {
    /// A new `deferred` row was written.
    Recorded(Box<ConflictResolution>),
    /// The work item already has an open deferral; nothing was written.
    AlreadyOpen(Box<ConflictResolution>),
}

/// Everything the deferral decision reads about one work item, taken in a
/// single snapshot.
#[derive(Debug, Clone, Default)]
pub struct ConflictDeferralState {
    /// The task exists, is not deleted and is `in_review`.
    pub in_review: bool,
    /// The open `deferred` row, if any.
    pub open: Option<ConflictResolution>,
    /// A resolution is in flight or owed: a `pending` or `running` attempt
    /// exists for the work item, or it carries an uncleared `merge_conflict`
    /// signal (a fix vehicle is running, or the item waits on a human for one).
    pub resolution_owed: bool,
    /// The last recorded poll already had this PR conflicting. `false` on the
    /// first conflicting probe after a mergeable (or never-polled) one, which
    /// is what marks a new conflict.
    pub was_conflicting: bool,
    /// The newest watcher-written row for this PR, deferral records included.
    pub latest: Option<ConflictResolution>,
}

impl ConflictDeferralState {
    /// `true` when the current conflict has already been handed to the
    /// resolver, so it must not be (re-)deferred: the newest row is a
    /// deferral that ended by releasing the conflict, or an attempt that has
    /// not succeeded. A conflict the last poll did not see is a new one and
    /// is never "already released".
    pub fn conflict_already_released(&self) -> bool {
        if !self.was_conflicting {
            return false;
        }
        match &self.latest {
            None => false,
            Some(row) if row.status == CONFLICT_STATUS_DEFERRED => false,
            Some(row) => match row.deferral_end_reason.as_deref() {
                Some(reason) => ConflictDeferralEnd::parse(reason).is_none_or(ConflictDeferralEnd::hands_to_resolver),
                None => row.status != "succeeded",
            },
        }
    }
}

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "deferral_arm",
            "ALTER TABLE conflict_resolutions ADD COLUMN deferral_arm TEXT",
        ),
        (
            "deferral_end_reason",
            "ALTER TABLE conflict_resolutions ADD COLUMN deferral_end_reason TEXT",
        ),
    ] {
        if !super::schema_init::table_has_column(conn, "conflict_resolutions", column)? {
            conn.execute_batch(ddl)?;
        }
    }
    // A Review card says when its conflict is deferred, so a deferral
    // starting or ending is a board input: journal it against the card, as
    // `board_projection_schema` does for every other input a card reads.
    conn.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS board_changes_conflict_deferral_insert
            AFTER INSERT ON conflict_resolutions WHEN NEW.status = '{CONFLICT_STATUS_DEFERRED}'
         BEGIN
            INSERT INTO board_changes(item_id, source) VALUES (NEW.work_item_id, 'conflict_resolutions');
         END;
         CREATE TRIGGER IF NOT EXISTS board_changes_conflict_deferral_update
            AFTER UPDATE OF status ON conflict_resolutions
            WHEN OLD.status = '{CONFLICT_STATUS_DEFERRED}' AND NEW.status IS NOT OLD.status
         BEGIN
            INSERT INTO board_changes(item_id, source) VALUES (NEW.work_item_id, 'conflict_resolutions');
         END;"
    ))?;
    Ok(())
}

fn query_open_deferral(conn: &Connection, work_item_id: &str) -> Result<Option<ConflictResolution>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CONFLICT_RESOLUTION_COLUMNS}
         FROM conflict_resolutions
         WHERE work_item_id = ?1 AND status = '{CONFLICT_STATUS_DEFERRED}'
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    ))?;
    Ok(stmt.query_row([work_item_id], map_conflict_resolution).optional()?)
}

/// `true` when `work_item_id` has an open deferred conflict. For readers
/// that only need the fact, such as the stuck report.
pub(super) fn has_open_deferral(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM conflict_resolutions
                           WHERE work_item_id = ?1 AND status = '{CONFLICT_STATUS_DEFERRED}')"
        ),
        [work_item_id],
        |row| row.get(0),
    )?)
}

/// The ids among `work_item_ids` that have an open deferred conflict, in one
/// statement however many ids are asked about.
pub(super) fn open_deferrals_among(conn: &Connection, work_item_ids: &[&str]) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT work_item_id FROM conflict_resolutions
         WHERE status = '{CONFLICT_STATUS_DEFERRED}'
           AND work_item_id IN (SELECT value FROM json_each(?1))"
    ))?;
    let ids = stmt
        .query_map([super::query_ensure::id_array(work_item_ids)], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(ids)
}

/// End the open deferral of `work_item_id`. Returns the ended row, or `None`
/// when nothing was deferred.
pub(super) fn end_deferral_in_tx(
    conn: &Connection,
    work_item_id: &str,
    end: ConflictDeferralEnd,
    now: &str,
) -> Result<Option<ConflictResolution>> {
    let Some(open) = query_open_deferral(conn, work_item_id)? else {
        return Ok(None);
    };
    conn.execute(
        &format!(
            "UPDATE conflict_resolutions
                SET status              = '{CONFLICT_STATUS_DEFERRAL_ENDED}',
                    deferral_end_reason = ?2,
                    finished_at         = COALESCE(finished_at, ?3)
              WHERE work_item_id = ?1 AND status = '{CONFLICT_STATUS_DEFERRED}'"
        ),
        params![work_item_id, end.as_str(), now],
    )?;
    query_conflict_resolution(conn, &open.id)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// The open deferred conflict for `work_item_id`, if any.
    pub fn open_conflict_deferral(&self, work_item_id: &str) -> Result<Option<ConflictResolution>> {
        let conn = self.connect()?;
        query_open_deferral(&conn, work_item_id)
    }

    /// One consistent read of everything the deferral decision depends on.
    pub fn conflict_deferral_state(&self, work_item_id: &str, pr_url: &str) -> Result<ConflictDeferralState> {
        let conn = self.connect()?;
        let task: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT status, pr_mergeable_state FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                [work_item_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((status, mergeable)) = task else {
            return Ok(ConflictDeferralState::default());
        };
        let resolution_owed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM conflict_resolutions
                            WHERE work_item_id = ?1 AND status IN ('pending', 'running'))
                 OR EXISTS(SELECT 1 FROM task_blocked_signals
                            WHERE work_item_id = ?1 AND reason = 'merge_conflict' AND cleared_at IS NULL)",
            [work_item_id],
            |row| row.get(0),
        )?;
        let latest = conn
            .prepare(&format!(
                "SELECT {CONFLICT_RESOLUTION_COLUMNS}
                 FROM conflict_resolutions
                 WHERE work_item_id = ?1 AND pr_url = ?2 AND event_source = 'review_watch'
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
            ))?
            .query_row(params![work_item_id, pr_url], map_conflict_resolution)
            .optional()?;
        Ok(ConflictDeferralState {
            in_review: status == "in_review",
            open: query_open_deferral(&conn, work_item_id)?,
            resolution_owed,
            was_conflicting: mergeable.as_deref() == Some("conflicting"),
            latest,
        })
    }

    /// Record that `input`'s conflict was observed and its resolution held
    /// back. Writes a `deferred` row in the treatment arm; touches nothing
    /// else, in particular not the task row, so a repeat probe of a deferred
    /// PR leaves no trace that could be read as activity.
    ///
    /// The row keeps the head it was observed at but not the base: the
    /// ledger's UNIQUE key is `(work_item_id, base_sha_at_trigger,
    /// head_sha_before)` and belongs to resolution attempts. A deferral
    /// record with a NULL base never takes that key, so it can always be
    /// written, and the attempt that later resolves the same base and head is
    /// never refused because of it. One open deferral per work item is
    /// enforced here instead, inside the write transaction.
    pub fn record_conflict_deferral(&self, input: ConflictResolutionInsertInput) -> Result<ConflictDeferralRecord> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(open) = query_open_deferral(&tx, &input.work_item_id)? {
            tx.commit()?;
            return Ok(ConflictDeferralRecord::AlreadyOpen(Box::new(open)));
        }
        let id = next_id("crz");
        tx.execute(
            &format!(
                "INSERT INTO conflict_resolutions
                    (id, product_id, work_item_id, pr_url, pr_number,
                     head_branch, base_branch, head_sha_before,
                     status, deferral_arm, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                         '{CONFLICT_STATUS_DEFERRED}', '{CONFLICT_DEFERRAL_ARM_DEFERRED}', ?9)"
            ),
            params![
                id,
                input.product_id,
                input.work_item_id,
                input.pr_url,
                input.pr_number,
                input.head_branch,
                input.base_branch,
                input.head_sha_before,
                now_string(),
            ],
        )?;
        let recorded = query_conflict_resolution(&tx, &id)?
            .with_context(|| format!("unknown conflict_resolution after deferral insert: {id}"))?;
        tx.commit()?;
        Ok(ConflictDeferralRecord::Recorded(Box::new(recorded)))
    }

    /// End the open deferral of `work_item_id`, recording why. Returns the
    /// ended row; `Ok(None)` when the item had no open deferral, so the call
    /// is safe to make unconditionally.
    pub fn end_conflict_deferral(
        &self,
        work_item_id: &str,
        end: ConflictDeferralEnd,
    ) -> Result<Option<ConflictResolution>> {
        let mut conn = self.connect()?;
        // Callers ask on every clean, merged or closed observation of every
        // PR, and almost none has a deferral: answer those without taking
        // the write lock.
        if query_open_deferral(&conn, work_item_id)?.is_none() {
            return Ok(None);
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ended = end_deferral_in_tx(&tx, work_item_id, end, &now_string())?;
        tx.commit()?;
        Ok(ended)
    }

    /// Label an attempt row with the rollout arm its PR was in when the
    /// conflict was detected. First label wins: a row keeps the arm it was
    /// actually handled under even if the flags change later.
    pub fn stamp_conflict_resolution_deferral_arm(&self, attempt_id: &str, arm: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE conflict_resolutions SET deferral_arm = ?2 WHERE id = ?1 AND deferral_arm IS NULL",
            params![attempt_id, arm],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product_named};
    use std::path::PathBuf;

    const PR: &str = "https://github.com/o/r/pull/7";

    fn db_with_review_item() -> (WorkDb, String, String) {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let product = create_test_product_named(&db, "Deferral");
        let task = create_test_chore_manual(&db, &product.id, "Conflicted PR");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
                params![task.id, PR],
            )
            .unwrap();
        (db, product.id, task.id)
    }

    fn input(product: &str, item: &str, head: &str) -> ConflictResolutionInsertInput {
        ConflictResolutionInsertInput::builder()
            .product_id(product)
            .work_item_id(item)
            .pr_url(PR)
            .pr_number(7)
            .head_branch("feature")
            .base_branch("main")
            .base_sha_at_trigger("base-1")
            .head_sha_before(head)
            .build()
    }

    fn recorded(record: ConflictDeferralRecord) -> ConflictResolution {
        match record {
            ConflictDeferralRecord::Recorded(row) => *row,
            other => panic!("expected a new deferral row, got {other:?}"),
        }
    }

    fn updated_at(db: &WorkDb, item: &str) -> String {
        db.connect()
            .unwrap()
            .query_row("SELECT updated_at FROM tasks WHERE id = ?1", [item], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_deferral_is_one_ledger_row_that_is_not_an_attempt() {
        let (db, product, item) = db_with_review_item();
        let before = updated_at(&db, &item);

        let row = recorded(db.record_conflict_deferral(input(&product, &item, "head-1")).unwrap());
        assert_eq!(row.status, CONFLICT_STATUS_DEFERRED);
        assert_eq!(row.deferral_arm.as_deref(), Some(CONFLICT_DEFERRAL_ARM_DEFERRED));
        assert_eq!(row.deferral_end_reason, None);
        assert_eq!(row.finished_at, None);
        assert_eq!(row.revision_task_id, None);

        // A second probe of the same deferred PR, even at a new head, adds nothing.
        let again = db.record_conflict_deferral(input(&product, &item, "head-2")).unwrap();
        assert_eq!(again, ConflictDeferralRecord::AlreadyOpen(Box::new(row.clone())));
        assert_eq!(
            db.list_conflict_resolutions(None, &[], Some(&item), None)
                .unwrap()
                .len(),
            1
        );

        // Nothing reads as a fix in flight, and the task row was not touched.
        assert!(db.active_conflict_resolution_for_work_item(&item).unwrap().is_none());
        assert_eq!(db.open_conflict_deferral(&item).unwrap().map(|r| r.id), Some(row.id));
        assert_eq!(updated_at(&db, &item), before);
        let state = db.conflict_deferral_state(&item, PR).unwrap();
        assert!(state.in_review && !state.resolution_owed);
    }

    #[test]
    fn ending_a_deferral_records_why_and_only_once() {
        let (db, product, item) = db_with_review_item();
        let row = recorded(db.record_conflict_deferral(input(&product, &item, "head-1")).unwrap());

        let ended = db
            .end_conflict_deferral(&item, ConflictDeferralEnd::MergeRequested)
            .unwrap()
            .expect("the open deferral ends");
        assert_eq!(ended.id, row.id);
        assert_eq!(ended.status, CONFLICT_STATUS_DEFERRAL_ENDED);
        assert_eq!(ended.deferral_end_reason.as_deref(), Some("merge_requested"));
        assert!(ended.finished_at.is_some());
        assert!(db.open_conflict_deferral(&item).unwrap().is_none());
        // Ending twice is a no-op, not a second record.
        assert!(
            db.end_conflict_deferral(&item, ConflictDeferralEnd::HumanRequest)
                .unwrap()
                .is_none()
        );
    }

    /// The ledger's UNIQUE key belongs to resolution attempts. A deferral
    /// record must neither take it (the attempt that resolves the same base
    /// and head would be refused) nor be refused by it (the conflict would be
    /// held with no record).
    #[test]
    fn a_deferral_never_competes_with_attempts_for_the_ledger_key() {
        let (db, product, item) = db_with_review_item();
        // An earlier attempt at this exact base and head still holds the key.
        let earlier = db
            .insert_conflict_resolution(input(&product, &item, "head-1"))
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_abandoned(&earlier.id, "revision_create_failed")
            .unwrap();

        let deferral = recorded(db.record_conflict_deferral(input(&product, &item, "head-1")).unwrap());
        assert_eq!(deferral.head_sha_before.as_deref(), Some("head-1"));
        assert_eq!(deferral.base_sha_at_trigger, None);

        // And while a deferral is open at a head, the attempt for that head
        // can still be inserted.
        let attempt = db
            .insert_conflict_resolution(input(&product, &item, "head-2"))
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_abandoned(&attempt.id, "manual_abandon")
            .unwrap();
        db.end_conflict_deferral(&item, ConflictDeferralEnd::ConflictCleared)
            .unwrap();
        recorded(db.record_conflict_deferral(input(&product, &item, "head-3")).unwrap());
        let resolving = db
            .insert_conflict_resolution(input(&product, &item, "head-3"))
            .unwrap()
            .expect("the open deferral does not hold the UNIQUE key");
        assert_eq!(resolving.status, "pending");
    }

    #[test]
    fn deferral_records_do_not_spend_the_churn_budget() {
        let (db, product, item) = db_with_review_item();
        // More deferral records than the churn guard allows attempts.
        for head in ["h1", "h2", "h3", "h4"] {
            recorded(db.record_conflict_deferral(input(&product, &item, head)).unwrap());
            db.end_conflict_deferral(&item, ConflictDeferralEnd::ConflictCleared)
                .unwrap()
                .unwrap();
        }
        let attempt = db
            .insert_conflict_resolution(input(&product, &item, "h5"))
            .unwrap()
            .unwrap();
        assert_eq!(attempt.status, "pending", "{:?}", attempt.failure_reason);
    }

    #[test]
    fn a_released_conflict_is_not_deferred_again_but_a_new_one_is() {
        let (db, product, item) = db_with_review_item();
        let conflicting = |value: &str| {
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET pr_mergeable_state = ?2 WHERE id = ?1",
                    params![item, value],
                )
                .unwrap();
        };
        // Nothing recorded yet: nothing has been released.
        conflicting("conflicting");
        assert!(
            !db.conflict_deferral_state(&item, PR)
                .unwrap()
                .conflict_already_released()
        );

        recorded(db.record_conflict_deferral(input(&product, &item, "h1")).unwrap());
        assert!(
            !db.conflict_deferral_state(&item, PR)
                .unwrap()
                .conflict_already_released()
        );

        // Released by a human: the same conflict stays released.
        db.end_conflict_deferral(&item, ConflictDeferralEnd::HumanRequest)
            .unwrap();
        assert!(
            db.conflict_deferral_state(&item, PR)
                .unwrap()
                .conflict_already_released()
        );

        // The attempt that follows keeps it released until it succeeds.
        let attempt = db
            .insert_conflict_resolution(input(&product, &item, "h1"))
            .unwrap()
            .unwrap();
        let state = db.conflict_deferral_state(&item, PR).unwrap();
        assert!(state.resolution_owed && state.conflict_already_released());
        db.mark_conflict_resolution_succeeded(&attempt.id, Some("h2")).unwrap();
        assert!(
            !db.conflict_deferral_state(&item, PR)
                .unwrap()
                .conflict_already_released()
        );

        // A deferral that ended because the PR went clean released nothing.
        recorded(db.record_conflict_deferral(input(&product, &item, "h2")).unwrap());
        db.end_conflict_deferral(&item, ConflictDeferralEnd::ConflictCleared)
            .unwrap();
        assert!(
            !db.conflict_deferral_state(&item, PR)
                .unwrap()
                .conflict_already_released()
        );

        // A conflict the last poll did not see is new, whatever came before.
        recorded(db.record_conflict_deferral(input(&product, &item, "h3")).unwrap());
        db.end_conflict_deferral(&item, ConflictDeferralEnd::MergeRequested)
            .unwrap();
        assert!(
            db.conflict_deferral_state(&item, PR)
                .unwrap()
                .conflict_already_released()
        );
        conflicting("mergeable");
        assert!(
            !db.conflict_deferral_state(&item, PR)
                .unwrap()
                .conflict_already_released()
        );
    }

    #[test]
    fn a_human_retry_of_a_deferred_row_releases_it() {
        let (db, product, item) = db_with_review_item();
        let row = recorded(db.record_conflict_deferral(input(&product, &item, "h1")).unwrap());
        let released = db
            .retry_conflict_resolution(&row.id)
            .unwrap()
            .expect("a deferred row can be retried");
        assert_eq!(released.status, CONFLICT_STATUS_DEFERRAL_ENDED);
        assert_eq!(released.deferral_end_reason.as_deref(), Some("human_request"));
        // The parent stays in review: the watcher starts the resolution on
        // its next pass, through the same path as any other conflict.
        let status: String = db
            .connect()
            .unwrap()
            .query_row("SELECT status FROM tasks WHERE id = ?1", [&item], |row| row.get(0))
            .unwrap();
        assert_eq!(status, "in_review");
    }

    /// The card's wait phrase is read through the incremental board
    /// projection, which only re-reads a card when one of its inputs is
    /// journaled as changed. A deferral starting or ending must be one.
    #[test]
    fn recording_and_ending_a_deferral_refreshes_the_board_card() {
        use crate::board_caches::BoardCaches;
        use crate::board_projection::CachedProjection;
        use boss_metrics::population_timing::PopulationTrace;

        let (db, product, item) = db_with_review_item();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET review_required_state = 'approved', ci_required_state = 'success' WHERE id = ?1",
                [&item],
            )
            .unwrap();
        let mut model: Option<CachedProjection> = None;
        let mut card_wait = || -> Option<String> {
            let (tree, _) = db
                .refresh_board_projection(
                    &product,
                    &mut model,
                    &mut PopulationTrace::disabled(),
                    &BoardCaches::default(),
                    100,
                )
                .unwrap();
            assert_eq!(
                serde_json::to_value(&tree).unwrap(),
                serde_json::to_value(db.get_work_tree_at(&product, 100).unwrap()).unwrap(),
                "the incremental board must match a full rebuild"
            );
            tree.task_runtimes
                .iter()
                .find(|runtime| runtime.work_item_id == item)
                .and_then(|runtime| runtime.dispatch_wait_reason.clone())
        };
        assert_eq!(card_wait().as_deref(), Some("awaiting_merge"));

        recorded(db.record_conflict_deferral(input(&product, &item, "h1")).unwrap());
        assert_eq!(card_wait().as_deref(), Some("conflict_deferred"));

        db.end_conflict_deferral(&item, ConflictDeferralEnd::ConflictCleared)
            .unwrap()
            .unwrap();
        assert_eq!(card_wait().as_deref(), Some("awaiting_merge"));
    }

    /// The stuck-work handling must see a deferred conflict for what it is:
    /// no fix in flight, and named in the stuck report.
    #[test]
    fn a_deferred_conflict_is_visible_to_stuck_work_handling_and_is_not_progress() {
        let (db, product, item) = db_with_review_item();
        // Each helper takes the connection for one statement only: the store
        // has a single connection, and the ledger calls below need it too.
        let no_inflight_ladder = || -> bool {
            db.connect()
                .unwrap()
                .query_row(
                    "SELECT c2_no_inflight_ladder FROM progression_candidate_clauses WHERE work_item_id = ?1",
                    [&item],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let classify = || -> String {
            let conn = db.connect().unwrap();
            let task = query_task(&conn, &item).unwrap().unwrap();
            crate::stuck_report::classification(&conn, &task, None).unwrap()
        };
        // The phrase a Review card shows for the item.
        let card_wait = || -> String {
            let conn = db.connect().unwrap();
            let task = query_task(&conn, &item).unwrap().unwrap();
            crate::unscheduled_wait::review_wait(&conn, &task)
                .unwrap()
                .label
                .unwrap()
        };
        let set = |assignments: &str| {
            db.connect()
                .unwrap()
                .execute(&format!("UPDATE tasks SET {assignments} WHERE id = ?1"), [&item])
                .unwrap();
        };
        set("review_required_state = 'approved', ci_required_state = 'success', pr_mergeable_state = 'conflicting'");
        assert_eq!(classify(), "awaiting_merge");
        assert_eq!(card_wait(), "waiting for its pull request to merge");

        recorded(db.record_conflict_deferral(input(&product, &item, "h1")).unwrap());
        // The card says what is going on: conflicted, resolved at merge.
        assert_eq!(
            card_wait(),
            "conflicted; the conflict will be resolved when a merge is requested"
        );
        assert!(
            no_inflight_ladder(),
            "a deferred conflict is not a resolution in flight"
        );
        assert_eq!(classify(), "awaiting_merge:conflict_deferred");

        // A review that is not finished is still reported as such, with the
        // deferral named beside it.
        set("review_required_state = 'required'");
        assert_eq!(classify(), "review_without_live_work:conflict_deferred");
        assert_eq!(card_wait(), "waiting for a human review decision");
        set("review_required_state = 'approved'");

        // By contrast, the attempt that follows a release *is* in flight.
        db.end_conflict_deferral(&item, ConflictDeferralEnd::MergeRequested)
            .unwrap();
        db.insert_conflict_resolution(input(&product, &item, "h1"))
            .unwrap()
            .unwrap();
        assert!(!no_inflight_ladder());
        assert_eq!(classify(), "awaiting_merge");
        assert_eq!(card_wait(), "waiting for its pull request to merge");
    }
}
