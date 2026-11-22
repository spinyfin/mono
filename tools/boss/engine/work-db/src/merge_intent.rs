//! `merge_intents`: a person's standing request to merge a work item's PR.
//!
//! `github_merge_intents` and `trunk_merge_intents` each track one submission
//! to a merge mechanism and are retired when that submission leaves the
//! queue. This table records the request behind those submissions, so it
//! survives a failed attempt: the row stays `active` while CI is fixed or a
//! conflict is resolved, and ends only on one of the events below.
//!
//! # What ends an intent
//!
//! Two triggers on `tasks` end the active intent in the same statement that
//! changes the card, so no writer of those columns can leave one standing:
//!
//! | Write to `tasks` | Status | Reason |
//! | --- | --- | --- |
//! | soft delete | `closed` | `task_deleted` |
//! | `pr_url` changes | `withdrawn` | `pr_rebound` |
//! | status becomes `done` / `archived` | `closed` | `task_finished` |
//! | status becomes anything but `in_review` / `blocked` | `withdrawn` | `moved_out_of_review` |
//! | `parked_at` is stamped | `invalidated` | `parked` |
//! | `review_required_state` becomes `changes_requested` | `invalidated` | `changes_requested` |
//! | blocked for a reason no automatic fix owns | `invalidated` | `blocked` |
//! | a revision that is not a CI fix or conflict resolution is inserted | `invalidated` | `non_remediation_revision` |
//!
//! `blocked: ci_failure`, `ci_failure_exhausted` and `merge_conflict` are the
//! engine fixing what ended the merge attempt, so they keep the intent.
//! The merge poller ends it as `merged` when it observes the merge, before
//! the card moves to `done` (see `retire_github_merge_intent`); a newer
//! request supersedes it; and the engine ends it when the item is mergeable
//! again and a person has to look first ([`WorkDb::end_merge_intent`]).
//! A failed-checks dequeue that will not mint a CI fix (maintenance off,
//! budget spent or zero, a human suppression, or an existing attempt for
//! the same merge-queue episode) ends the intent immediately rather than
//! leave it standing for a fix that never starts.
//!
//! # What the card shows
//!
//! `card_merge_intents` exposes at most one row per work item, the one with
//! `on_card = 1`, as the JSON `Task::merge_intent` deserialises. An intent
//! that ended needing attention stays on the card until the card leaves
//! review, is bound to another PR, a merge is requested again, a new
//! revision is inserted, or the PR head moves past the head the ending
//! compared.

use super::*;
use boss_protocol::{
    CREATED_VIA_CI_FIX_PREFIX, CREATED_VIA_MERGE_CONFLICT_PREFIX, MERGE_INTENT_ACTIVE, MERGE_INTENT_CLOSED,
    MERGE_INTENT_INVALIDATED, MERGE_INTENT_LAPSED, MERGE_INTENT_MERGED, MERGE_INTENT_NEEDS_REVIEW,
    MERGE_INTENT_REASON_BLOCKED, MERGE_INTENT_REASON_CHANGES_REQUESTED, MERGE_INTENT_REASON_MERGED,
    MERGE_INTENT_REASON_MOVED_OUT_OF_REVIEW, MERGE_INTENT_REASON_NEW_REQUEST, MERGE_INTENT_REASON_NO_DECISION,
    MERGE_INTENT_REASON_NON_REMEDIATION_REVISION, MERGE_INTENT_REASON_PARKED, MERGE_INTENT_REASON_PR_REBOUND,
    MERGE_INTENT_REASON_REMOVED_FROM_MERGE_QUEUE, MERGE_INTENT_REASON_TASK_DELETED, MERGE_INTENT_REASON_TASK_FINISHED,
    MERGE_INTENT_SUPERSEDED, MERGE_INTENT_WITHDRAWN, MergeIntent,
};

pub(super) const MIGRATION_ID: &str = "20261118_2035_merge_intents";

/// The row as the JSON `MergeIntent` deserialises. Shared by the card view
/// and every read here, so a row is never mapped two different ways.
const INTENT_JSON: &str = "json_object(
    'id', id, 'work_item_id', work_item_id, 'pr_url', pr_url, 'status', status,
    'approved_head_sha', approved_head_sha, 'decided_head_sha', decided_head_sha,
    'reason', reason, 'detail', detail, 'automatic_merges', automatic_merges,
    'requested_at', created_at, 'ended_at', ended_at)";

/// `blocked_reason` values that mean the engine is fixing what ended the
/// merge attempt. Any other block is not an automatic fix's to clear.
const REMEDIATION_BLOCKED_REASONS: &str = "'merge_conflict', 'ci_failure', 'ci_failure_exhausted'";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS merge_intents (
            id                TEXT PRIMARY KEY,
            work_item_id      TEXT NOT NULL,
            pr_url            TEXT NOT NULL,
            status            TEXT NOT NULL,
            approved_head_sha TEXT,
            decided_head_sha  TEXT,
            reason            TEXT,
            detail            TEXT,
            automatic_merges  INTEGER NOT NULL DEFAULT 0,
            fix_owed_since    TEXT,
            on_card           INTEGER NOT NULL DEFAULT 1,
            created_at        TEXT NOT NULL,
            ended_at          TEXT
         );
         CREATE UNIQUE INDEX IF NOT EXISTS merge_intents_active_idx
            ON merge_intents(work_item_id) WHERE status = '{MERGE_INTENT_ACTIVE}';
         CREATE UNIQUE INDEX IF NOT EXISTS merge_intents_on_card_idx
            ON merge_intents(work_item_id) WHERE on_card = 1;
         CREATE INDEX IF NOT EXISTS merge_intents_by_item
            ON merge_intents(work_item_id, created_at);
         DROP VIEW IF EXISTS card_merge_intents;
         CREATE VIEW card_merge_intents AS
            SELECT work_item_id, {INTENT_JSON} AS view_json FROM merge_intents WHERE on_card = 1;
         DROP TRIGGER IF EXISTS merge_intents_follow_task;
         DROP TRIGGER IF EXISTS merge_intents_end_on_revision;
         DROP TRIGGER IF EXISTS merge_intents_clear_ended_on_revision;
         DROP TRIGGER IF EXISTS merge_intents_clear_ended_on_head_move;
         {FOLLOW_TASK_TRIGGER}
         {REVISION_TRIGGER}
         {CLEAR_ENDED_ON_REVISION}
         {CLEAR_ENDED_ON_HEAD_MOVE}",
        FOLLOW_TASK_TRIGGER = follow_task_trigger(),
        REVISION_TRIGGER = legacy_revision_trigger(),
        CLEAR_ENDED_ON_REVISION = clear_ended_on_revision_trigger(),
        CLEAR_ENDED_ON_HEAD_MOVE = clear_ended_on_head_move_trigger(),
    ))?;
    install_journal_triggers(conn)
}

/// Frozen body of shipped migration 20261118_2035; do not edit.
/// Replaced by `migrate_dequeue_tracking`.
fn legacy_revision_trigger() -> String {
    format!(
        "CREATE TRIGGER IF NOT EXISTS merge_intents_end_on_revision
         AFTER INSERT ON tasks
         WHEN NEW.kind = 'revision' AND NEW.parent_task_id IS NOT NULL
          AND NEW.created_via NOT LIKE '{CREATED_VIA_CI_FIX_PREFIX}%'
          AND NEW.created_via NOT LIKE '{CREATED_VIA_MERGE_CONFLICT_PREFIX}%'
         BEGIN
            UPDATE merge_intents SET
                status = '{MERGE_INTENT_INVALIDATED}',
                reason = '{MERGE_INTENT_REASON_NON_REMEDIATION_REVISION}',
                detail = 'Merge request ended: a revision started that is not an automatic CI or conflict fix. Review the result, then merge again.',
                ended_at = strftime('%s', 'now'),
                fix_owed_since = NULL
            WHERE work_item_id = NEW.parent_task_id AND status = '{MERGE_INTENT_ACTIVE}';
         END;"
    )
}

/// Frozen body of shipped migration 20261118_2035; do not edit.
/// Replaced by `migrate_dequeue_tracking`.
fn clear_ended_on_revision_trigger() -> String {
    format!(
        "CREATE TRIGGER IF NOT EXISTS merge_intents_clear_ended_on_revision
         AFTER INSERT ON tasks
         WHEN NEW.kind = 'revision' AND NEW.parent_task_id IS NOT NULL
         BEGIN
            UPDATE merge_intents SET on_card = 0
            WHERE work_item_id = NEW.parent_task_id AND on_card = 1
              AND status != '{MERGE_INTENT_ACTIVE}';
         END;"
    )
}

/// Upgrade existing databases without relying on same-event trigger order.
pub(super) fn migrate_dequeue_tracking(conn: &Connection) -> Result<()> {
    if !crate::schema_init::table_has_column(conn, "merge_intents", "last_dequeue_at")? {
        conn.execute_batch("ALTER TABLE merge_intents ADD COLUMN last_dequeue_at INTEGER")?;
    }
    conn.execute_batch(&format!(
        "DROP TRIGGER IF EXISTS merge_intents_end_on_revision;
         DROP TRIGGER IF EXISTS merge_intents_clear_ended_on_revision;
         {}",
        revision_trigger()
    ))?;
    Ok(())
}

/// A card that leaves review, changes PR, is parked, is asked for changes or
/// is blocked on something else no longer carries the request it was given.
/// The second statement takes any intent off a card it no longer describes.
fn follow_task_trigger() -> String {
    let left_review = "NEW.status NOT IN ('in_review', 'blocked')";
    let parked = "(NEW.parked_at IS NOT NULL AND OLD.parked_at IS NULL)";
    let changes_requested = "(NEW.review_required_state = 'changes_requested' \
         AND OLD.review_required_state IS NOT 'changes_requested')";
    let other_block = format!(
        "(NEW.status = 'blocked' AND COALESCE(NEW.blocked_reason, '') NOT IN ({REMEDIATION_BLOCKED_REASONS}) \
         AND (NEW.status IS NOT OLD.status OR NEW.blocked_reason IS NOT OLD.blocked_reason))"
    );
    format!(
        "CREATE TRIGGER IF NOT EXISTS merge_intents_follow_task
         AFTER UPDATE OF status, blocked_reason, deleted_at, pr_url, parked_at, review_required_state ON tasks
         WHEN EXISTS (SELECT 1 FROM merge_intents WHERE work_item_id = NEW.id AND on_card = 1)
         BEGIN
            UPDATE merge_intents SET
                status = CASE
                    WHEN NEW.deleted_at IS NOT NULL THEN '{MERGE_INTENT_CLOSED}'
                    WHEN NEW.pr_url IS NOT OLD.pr_url THEN '{MERGE_INTENT_WITHDRAWN}'
                    WHEN NEW.status IN ('done', 'archived') THEN '{MERGE_INTENT_CLOSED}'
                    WHEN {left_review} THEN '{MERGE_INTENT_WITHDRAWN}'
                    ELSE '{MERGE_INTENT_INVALIDATED}' END,
                reason = CASE
                    WHEN NEW.deleted_at IS NOT NULL THEN '{MERGE_INTENT_REASON_TASK_DELETED}'
                    WHEN NEW.pr_url IS NOT OLD.pr_url THEN '{MERGE_INTENT_REASON_PR_REBOUND}'
                    WHEN NEW.status IN ('done', 'archived') THEN '{MERGE_INTENT_REASON_TASK_FINISHED}'
                    WHEN {left_review} THEN '{MERGE_INTENT_REASON_MOVED_OUT_OF_REVIEW}'
                    WHEN {parked} THEN '{MERGE_INTENT_REASON_PARKED}'
                    WHEN {changes_requested} THEN '{MERGE_INTENT_REASON_CHANGES_REQUESTED}'
                    ELSE '{MERGE_INTENT_REASON_BLOCKED}' END,
                detail = CASE
                    WHEN NEW.deleted_at IS NOT NULL OR NEW.pr_url IS NOT OLD.pr_url OR {left_review} THEN NULL
                    WHEN {parked} THEN 'Merge request ended: this item was parked. Merge again when it is ready.'
                    WHEN {changes_requested}
                        THEN 'Merge request ended: a reviewer requested changes. Merge again once they are addressed.'
                    ELSE 'Merge request ended: this item became blocked on something an automatic fix does not handle. Merge again once it is unblocked.' END,
                ended_at = strftime('%s', 'now'),
                fix_owed_since = NULL
            WHERE work_item_id = NEW.id AND status = '{MERGE_INTENT_ACTIVE}'
              AND (NEW.deleted_at IS NOT NULL OR NEW.pr_url IS NOT OLD.pr_url OR {left_review}
                   OR {parked} OR {changes_requested} OR {other_block});
            UPDATE merge_intents SET on_card = 0
            WHERE work_item_id = NEW.id AND on_card = 1
              AND (NEW.deleted_at IS NOT NULL OR NEW.pr_url IS NOT OLD.pr_url OR {left_review});
         END;"
    )
}

/// A revision that is neither a CI fix nor a conflict resolution changes the
/// PR for a reason the request never covered: review feedback, a comment, a
/// person asking for more. Revisions always parent to the chain root, which
/// is the row an intent is recorded against.
fn revision_trigger() -> String {
    format!(
        "CREATE TRIGGER IF NOT EXISTS merge_intents_end_on_revision
         AFTER INSERT ON tasks
         WHEN NEW.kind = 'revision' AND NEW.parent_task_id IS NOT NULL
         BEGIN
            UPDATE merge_intents SET on_card = 0
            WHERE work_item_id = NEW.parent_task_id AND on_card = 1
              AND status != '{MERGE_INTENT_ACTIVE}';
            UPDATE merge_intents SET
                status = '{MERGE_INTENT_INVALIDATED}',
                reason = '{MERGE_INTENT_REASON_NON_REMEDIATION_REVISION}',
                detail = 'Merge request ended: a revision started that is not an automatic CI or conflict fix. Review the result, then merge again.',
                ended_at = strftime('%s', 'now'),
                fix_owed_since = NULL
            WHERE work_item_id = NEW.parent_task_id AND status = '{MERGE_INTENT_ACTIVE}'
              AND NEW.created_via NOT LIKE '{CREATED_VIA_CI_FIX_PREFIX}%'
              AND NEW.created_via NOT LIKE '{CREATED_VIA_MERGE_CONFLICT_PREFIX}%';
         END;"
    )
}

/// Once the PR head has moved past the one the ending compared, the
/// sentence names a pair that is no longer current.
fn clear_ended_on_head_move_trigger() -> String {
    format!(
        "CREATE TRIGGER IF NOT EXISTS merge_intents_clear_ended_on_head_move
         AFTER UPDATE OF pr_head_sha ON tasks
         WHEN NEW.pr_head_sha IS NOT OLD.pr_head_sha
          AND NULLIF(NEW.pr_head_sha, '') IS NOT NULL
         BEGIN
            UPDATE merge_intents SET on_card = 0
            WHERE work_item_id = NEW.id AND on_card = 1
              AND status != '{MERGE_INTENT_ACTIVE}'
              AND decided_head_sha IS NOT NULL
              AND decided_head_sha IS NOT NEW.pr_head_sha;
         END;"
    )
}

/// Board assembly reads `card_merge_intents`, so every write to the table is
/// a board input change. `board_projection_schema::migrate` re-installs
/// these whenever it rebuilds the journal triggers.
pub(super) fn install_journal_triggers(conn: &Connection) -> Result<()> {
    // The journal's own migration runs before this table exists.
    if !table_exists(conn, "merge_intents")? {
        return Ok(());
    }
    for (operation, row) in [("INSERT", "NEW"), ("UPDATE", "NEW"), ("DELETE", "OLD")] {
        conn.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS board_changes_merge_intents_{operation}
             AFTER {operation} ON merge_intents BEGIN
             INSERT INTO board_changes(item_id, source) VALUES ({row}.work_item_id, 'merge_intents');
             END;"
        ))?;
    }
    Ok(())
}

/// `Task::merge_intent` from a task row. The column is optional: a SELECT
/// that does not feed a card or a single-item read omits it and reports no
/// intent.
pub(crate) fn card_column(row: &Row<'_>) -> rusqlite::Result<Option<MergeIntent>> {
    let Ok(index) = row.as_ref().column_index("merge_intent") else {
        return Ok(None);
    };
    row.get::<_, Option<String>>(index)?
        .map(|json| parse(index, &json))
        .transpose()
}

fn parse(index: usize, json: &str) -> rusqlite::Result<MergeIntent> {
    serde_json::from_str(json)
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err)))
}

fn query_intent(conn: &Connection, filter: &str, params: impl rusqlite::Params) -> Result<Option<MergeIntent>> {
    conn.query_row(
        &format!("SELECT {INTENT_JSON} FROM merge_intents WHERE {filter}"),
        params,
        |row| parse(0, &row.get::<_, String>(0)?),
    )
    .optional()
    .map_err(Into::into)
}

/// How an active intent ends when the engine, not a write to `tasks`, ends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeIntentEnd {
    /// One of the `MERGE_INTENT_*` status constants other than `active`.
    pub status: &'static str,
    /// One of the `MERGE_INTENT_REASON_*` codes.
    pub reason: &'static str,
    /// The sentence the card shows.
    pub detail: Option<String>,
    /// The PR head compared against the approved one, when a comparison was
    /// what ended the intent.
    pub decided_head_sha: Option<String>,
}

/// Whether an ended intent still explains something on its card. Endings the
/// card itself already shows (merged, closed, withdrawn, replaced) do not.
fn stays_on_card(status: &str) -> bool {
    !matches!(
        status,
        MERGE_INTENT_MERGED | MERGE_INTENT_CLOSED | MERGE_INTENT_WITHDRAWN | MERGE_INTENT_SUPERSEDED
    )
}

/// End one intent, if it still stands. Every ending made in Rust goes
/// through here, addressed by id.
fn end_by_id(conn: &Connection, intent_id: &str, end: &MergeIntentEnd) -> Result<bool> {
    Ok(conn.execute(
        &format!(
            "UPDATE merge_intents
             SET status = ?2, reason = ?3, detail = ?4, decided_head_sha = COALESCE(?5, decided_head_sha),
                 on_card = ?6, ended_at = ?7, fix_owed_since = NULL
             WHERE id = ?1 AND status = '{MERGE_INTENT_ACTIVE}'"
        ),
        params![
            intent_id,
            end.status,
            end.reason,
            end.detail,
            end.decided_head_sha,
            stays_on_card(end.status),
            now_string()
        ],
    )? > 0)
}

fn query_active(conn: &Connection, work_item_id: &str, pr_url: &str) -> Result<Option<MergeIntent>> {
    query_intent(
        conn,
        &format!("work_item_id = ?1 AND pr_url = ?2 AND status = '{MERGE_INTENT_ACTIVE}'"),
        params![work_item_id, pr_url],
    )
}

/// The merge poller observed `pr_url` merge or close. Runs before the card
/// moves to `done`, so the intent records which of the two happened rather
/// than the trigger's generic `task_finished`. Scoped to the PR observed: a
/// late probe of a PR the item is no longer bound to ends nothing.
pub(crate) fn end_on_pr_terminal(conn: &Connection, work_item_id: &str, pr_url: &str, merged: bool) -> Result<bool> {
    let Some(intent) = query_active(conn, work_item_id, pr_url)? else {
        return Ok(false);
    };
    let end = MergeIntentEnd {
        status: if merged {
            MERGE_INTENT_MERGED
        } else {
            MERGE_INTENT_CLOSED
        },
        reason: if merged {
            MERGE_INTENT_REASON_MERGED
        } else {
            MERGE_INTENT_REASON_TASK_FINISHED
        },
        detail: None,
        decided_head_sha: None,
    };
    end_by_id(conn, &intent.id, &end)
}

/// What the engine knows about fixes made since a merge was requested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeIntentRemediation {
    /// The merge attempt left the queue on failed checks and no CI fix has
    /// been started for it yet. One is owed; nothing can be concluded until
    /// it exists. An existing merge-queue attempt for the same head, even
    /// one created before this dequeue, counts as handling the obligation.
    pub fix_owed: bool,
    /// Seconds since `fix_owed_since` when [`Self::fix_owed`] is true.
    pub fix_owed_age_secs: Option<i64>,
    /// CI-fix attempts started since the request.
    pub ci_fixes: usize,
    /// Conflict resolutions started since the request.
    pub conflict_resolutions: usize,
}

/// Why a failed-checks dequeue will not mint a CI-fix attempt. The standing
/// merge request ends instead of waiting for a row that never arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeIntentCiFixDecline {
    /// The product (or PR label) has automatic PR maintenance turned off.
    MaintenanceDisabled,
    /// The CI-fix budget is zero or already spent.
    BudgetExhausted,
    /// A `ci_remediations` row already exists for this merge-queue episode.
    EpisodeAlreadyHandled,
    /// A person suppressed CI remediation for this head.
    Suppressed,
}

impl MergeIntentCiFixDecline {
    fn detail(self) -> &'static str {
        match self {
            Self::MaintenanceDisabled => {
                "Review before merging again: automatic PR maintenance is off, so the merge that failed checks will not be retried."
            }
            Self::BudgetExhausted => {
                "Review before merging again: the CI-fix budget is spent, so the merge that failed checks will not be retried automatically."
            }
            Self::EpisodeAlreadyHandled => {
                "Review before merging again: this PR head already had a CI fix after a merge-queue failure. Merge again to retry."
            }
            Self::Suppressed => {
                "Review before merging again: automatic CI fixes are suppressed for this PR head. Merge again when you want them retried."
            }
        }
    }

    fn end(self, decided_head_sha: Option<String>) -> MergeIntentEnd {
        MergeIntentEnd {
            status: MERGE_INTENT_NEEDS_REVIEW,
            reason: MERGE_INTENT_REASON_NO_DECISION,
            detail: Some(self.detail().to_owned()),
            decided_head_sha,
        }
    }
}

/// What a merge-queue departure meant for the standing request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeQueueDeparture {
    /// No request stands for this PR.
    NoIntent,
    /// The request stands and the engine is expected to fix the cause.
    Standing,
    /// Nothing the engine fixes removed the PR, so the request ended.
    Ended,
}

/// SQL predicate: does `table`'s row belong to the merge-queue episode of the
/// PR head named by the SQL expression `head`? The owed-fix check uses it
/// to treat an existing same-head attempt as handling the obligation.
fn episode_head_match_sql(table: &str, head: &str) -> String {
    format!(
        "({table}.head_sha_at_trigger = 'mq:' || {head} OR {table}.head_sha_at_trigger = {head} OR {table}.before_commit_sha = {head})"
    )
}

/// Whether the failed-checks fix this intent is owed is still unfulfilled,
/// and how long the obligation has stood. Owed means the intent was stamped
/// at a failed-checks dequeue and no CI attempt has been minted for it: none
/// created at or after the stamp, and none already keyed to the PR head's
/// merge-queue episode.
fn fix_owed_on(conn: &Connection, intent: &MergeIntent) -> Result<(bool, Option<i64>)> {
    let approved_match = episode_head_match_sql("cr", "mi.approved_head_sha");
    let pr_head_match = episode_head_match_sql("cr", "t.pr_head_sha");
    let (owed, age): (bool, Option<i64>) = conn.query_row(
        &format!(
            "SELECT
                mi.fix_owed_since IS NOT NULL
                  AND NOT EXISTS (
                    SELECT 1 FROM ci_remediations cr
                    WHERE cr.work_item_id = mi.work_item_id
                      AND (
                        CAST(cr.created_at AS INTEGER) >= CAST(mi.fix_owed_since AS INTEGER)
                        OR (
                          cr.failure_kind = 'merge_queue_rebounce'
                          AND (
                            (NULLIF(mi.approved_head_sha, '') IS NOT NULL AND {approved_match})
                            OR EXISTS (
                                SELECT 1 FROM tasks t
                                 WHERE t.id = mi.work_item_id
                                   AND NULLIF(t.pr_head_sha, '') IS NOT NULL
                                   AND {pr_head_match}))))),
                CASE WHEN mi.fix_owed_since IS NOT NULL
                     THEN CAST(strftime('%s', 'now') AS INTEGER) - CAST(mi.fix_owed_since AS INTEGER)
                     ELSE NULL END
             FROM merge_intents mi WHERE mi.id = ?1"
        ),
        [&intent.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((owed, if owed { age } else { None }))
}

/// End a request that is still owed a CI fix the rebounce path declined to
/// mint. A request whose fix was already minted (this sweep or an earlier
/// one) is left alone, which is why a repeated decline on the same episode
/// is harmless.
fn end_owed_ci_fix(conn: &Connection, work_item_id: &str, pr_url: &str, why: MergeIntentCiFixDecline) -> Result<bool> {
    let Some(intent) = query_active(conn, work_item_id, pr_url)? else {
        return Ok(false);
    };
    // Ownership is by creation time: an attempt minted since the stamp is
    // this request's own fix. An older attempt on the same head is the very
    // reason the decline fired, so it must not keep the request standing.
    let own_fix_minted: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM ci_remediations cr, merge_intents mi
              WHERE mi.id = ?1 AND cr.work_item_id = mi.work_item_id
                AND CAST(cr.created_at AS INTEGER) >= CAST(mi.fix_owed_since AS INTEGER))",
        [&intent.id],
        |row| row.get(0),
    )?;
    let stamped: bool = conn.query_row(
        "SELECT fix_owed_since IS NOT NULL FROM merge_intents WHERE id = ?1",
        [&intent.id],
        |row| row.get(0),
    )?;
    if !stamped || own_fix_minted {
        return Ok(false);
    }
    let head = intent.approved_head_sha.clone();
    end_by_id(conn, &intent.id, &why.end(head))
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Record that a person asked for this item's PR to be merged. A request
    /// already standing for the item is superseded: asking again is a fresh
    /// approval of the PR as it is now. `approved_head_sha` falls back to the
    /// head the merge poller last stored for the item.
    pub fn record_merge_intent(
        &self,
        work_item_id: &str,
        pr_url: &str,
        approved_head_sha: Option<&str>,
    ) -> Result<MergeIntent> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_string();
        tx.execute(
            &format!(
                "UPDATE merge_intents
                 SET status = '{MERGE_INTENT_SUPERSEDED}', reason = '{MERGE_INTENT_REASON_NEW_REQUEST}',
                     ended_at = ?2, fix_owed_since = NULL
                 WHERE work_item_id = ?1 AND status = '{MERGE_INTENT_ACTIVE}'"
            ),
            params![work_item_id, now],
        )?;
        tx.execute(
            "UPDATE merge_intents SET on_card = 0 WHERE work_item_id = ?1 AND on_card = 1",
            [work_item_id],
        )?;
        let id = next_id("mi");
        tx.execute(
            &format!(
                "INSERT INTO merge_intents (id, work_item_id, pr_url, status, approved_head_sha, created_at)
                 VALUES (?1, ?2, ?3, '{MERGE_INTENT_ACTIVE}',
                         COALESCE(NULLIF(?4, ''), (SELECT NULLIF(pr_head_sha, '') FROM tasks WHERE id = ?2)), ?5)"
            ),
            params![id, work_item_id, pr_url, approved_head_sha, now],
        )?;
        let intent = query_intent(&tx, "id = ?1", [&id])?.context("merge intent vanished after insert")?;
        tx.commit()?;
        Ok(intent)
    }

    /// The request standing for this exact item and PR.
    pub fn active_merge_intent(&self, work_item_id: &str, pr_url: &str) -> Result<Option<MergeIntent>> {
        let conn = self.connect()?;
        query_active(&conn, work_item_id, pr_url)
    }

    /// Work items with a merge attempt in flight: an active merge request,
    /// an active GitHub-native auto-merge request or an active Trunk queue
    /// episode. The merge poller probes these PRs in full on every pass,
    /// whatever else it knows about them, because their next transition is
    /// the merge itself.
    ///
    /// Each arm filters on `status = 'active'`, the predicate of the partial
    /// unique index on that table, so the cost follows the number of merges
    /// in flight, not the history of merges.
    pub fn list_work_item_ids_with_pending_merge(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT work_item_id FROM merge_intents
              WHERE status = '{MERGE_INTENT_ACTIVE}'
             UNION
             SELECT work_item_id FROM github_merge_intents
              WHERE status = 'active'
             UNION
             SELECT work_item_id FROM trunk_merge_intents
              WHERE status = 'active'"
        ))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(collect_rows(rows)?.into_iter().collect())
    }

    /// End one intent, if it still stands. Addressed by id so a decision
    /// made about one request can never end the request that replaced it.
    pub fn end_merge_intent(&self, intent_id: &str, end: &MergeIntentEnd) -> Result<bool> {
        let conn = self.connect()?;
        end_by_id(&conn, intent_id, end)
    }

    /// The PR's merge attempt left GitHub's merge queue for `queue_reason`.
    /// Failed checks and a merge conflict are what the engine fixes, so the
    /// request stands through them unless a CI fix will not be minted (see
    /// [`MergeIntentCiFixDecline`]). Anything else (someone removed it, the
    /// queue was cleared, a reason this engine does not know) is not the
    /// engine's to undo and ends the request rather than leave it standing
    /// for a later fix to act on.
    pub fn note_merge_intent_left_queue(
        &self,
        work_item_id: &str,
        pr_url: &str,
        queue_reason: &str,
        event_created_at: i64,
    ) -> Result<MergeQueueDeparture> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(intent) = query_active(&tx, work_item_id, pr_url)? else {
            return Ok(MergeQueueDeparture::NoIntent);
        };
        // Timeline replay must not revive an old obligation or withdraw a newer request.
        // This ordering assumes the host's UTC clock is synchronized with GitHub:
        // requests and remediations use local time, but dequeues use GitHub time.
        // Clock skew can reject a fresh dequeue or misattribute a fix.
        let observed = tx.execute(
            "UPDATE merge_intents SET last_dequeue_at = ?2 WHERE id = ?1
             AND CAST(created_at AS INTEGER) < ?2
             AND (last_dequeue_at IS NULL OR last_dequeue_at < ?2)",
            params![intent.id, event_created_at],
        )?;
        if observed == 0 {
            return Ok(MergeQueueDeparture::Standing);
        }
        let departure = if queue_reason.eq_ignore_ascii_case("failed_checks") {
            // The rebounce producer decides whether a fix is minted, through
            // the real maintenance, budget, suppression and episode gates, and
            // ends the request when it declines. Here the engine only records
            // that a fix is owed.
            tx.execute(
                "UPDATE merge_intents SET fix_owed_since = ?2 WHERE id = ?1",
                params![intent.id, event_created_at.to_string()],
            )?;
            MergeQueueDeparture::Standing
        } else if queue_reason.eq_ignore_ascii_case("merge_conflict") {
            MergeQueueDeparture::Standing
        } else {
            let end = MergeIntentEnd {
                status: MERGE_INTENT_LAPSED,
                reason: MERGE_INTENT_REASON_REMOVED_FROM_MERGE_QUEUE,
                detail: Some(format!(
                    "Merge again when ready: the PR was removed from the merge queue ({queue_reason})."
                )),
                decided_head_sha: None,
            };
            end_by_id(&tx, &intent.id, &end)?;
            MergeQueueDeparture::Ended
        };
        tx.commit()?;
        Ok(departure)
    }

    /// End a standing request that is waiting for a CI fix that the rebounce
    /// path has declined to mint. No-ops unless `fix_owed_since` is set, so
    /// a conflict-only request or a request whose fix already started is
    /// left alone.
    pub fn decline_owed_merge_intent_ci_fix(
        &self,
        work_item_id: &str,
        pr_url: &str,
        why: MergeIntentCiFixDecline,
    ) -> Result<bool> {
        let conn = self.connect()?;
        end_owed_ci_fix(&conn, work_item_id, pr_url, why)
    }

    /// Fixes the engine has started for this item since `intent` was
    /// requested, and whether one is still owed. Deferred and predicted
    /// conflict rows are records of a conflict, not attempts to resolve one.
    pub fn merge_intent_remediation(&self, intent: &MergeIntent) -> Result<MergeIntentRemediation> {
        let conn = self.connect()?;
        let count = |sql: &str| -> Result<usize> {
            Ok(
                conn.query_row(sql, params![intent.work_item_id, intent.requested_at], |row| {
                    row.get::<_, i64>(0)
                })? as usize,
            )
        };
        let ci_fixes = count(
            "SELECT COUNT(*) FROM ci_remediations
             WHERE work_item_id = ?1 AND CAST(created_at AS INTEGER) >= CAST(?2 AS INTEGER)",
        )?;
        let conflict_resolutions = count(
            "SELECT COUNT(*) FROM conflict_resolutions
             WHERE work_item_id = ?1 AND status NOT IN ('deferred', 'predicted')
               AND CAST(created_at AS INTEGER) >= CAST(?2 AS INTEGER)",
        )?;
        let (fix_owed, fix_owed_age_secs) = fix_owed_on(&conn, intent)?;
        Ok(MergeIntentRemediation {
            fix_owed,
            fix_owed_age_secs,
            ci_fixes,
            conflict_resolutions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{MERGE_INTENT_NEEDS_REVIEW, MERGE_INTENT_REASON_NO_DECISION};

    const PR: &str = "https://github.com/example/repo/pull/42";

    struct Fixture {
        db: WorkDb,
        product: String,
        task: String,
    }

    /// A chore in review on [`PR`] whose last probed head is `head-1`.
    fn fixture() -> Fixture {
        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = crate::test_support::create_test_product_named(&db, "Merge intent");
        let task = crate::test_support::create_test_chore_manual(&db, product.id.clone(), "Ship it");
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(PR.into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET pr_head_sha = 'head-1' WHERE id = ?1", [&task.id])
            .unwrap();
        Fixture {
            db,
            product: product.id,
            task: task.id,
        }
    }

    impl Fixture {
        fn request(&self) -> MergeIntent {
            let intent = self.db.record_merge_intent(&self.task, PR, Some("head-1")).unwrap();
            self.db
                .connect()
                .unwrap()
                .execute(
                    "UPDATE merge_intents SET created_at = CAST(created_at AS INTEGER) - 2 WHERE id = ?1",
                    [&intent.id],
                )
                .unwrap();
            self.stored(&intent.id)
        }

        fn stored(&self, id: &str) -> MergeIntent {
            query_intent(&self.db.connect().unwrap(), "id = ?1", [id])
                .unwrap()
                .unwrap()
        }

        /// What the board would put on the card.
        fn on_card(&self) -> Option<MergeIntent> {
            self.db
                .get_work_tree(&self.product)
                .unwrap()
                .chores
                .into_iter()
                .find(|task| task.id == self.task)
                .unwrap()
                .merge_intent
        }

        fn patch(&self, patch: WorkItemPatch) {
            self.db.update_work_item(&self.task, patch).unwrap();
        }

        fn revision(&self, created_via: &str) {
            self.db
                .create_revision(
                    CreateRevisionInput::builder()
                        .parent_task_id(&self.task)
                        .description(format!("Follow-up from {created_via}"))
                        .created_via(created_via)
                        .build(),
                    &FakePrStateChecker::always(PrOpenState::Open),
                )
                .unwrap();
        }
    }

    #[test]
    fn a_request_is_recorded_against_the_approved_head_and_shown_on_the_card() {
        let f = fixture();
        assert!(f.on_card().is_none(), "no request, nothing on the card");
        let intent = f.request();
        assert!(intent.is_active());
        assert_eq!(intent.approved_head_sha.as_deref(), Some("head-1"));
        assert_eq!(f.db.active_merge_intent(&f.task, PR).unwrap().unwrap().id, intent.id);
        assert_eq!(f.on_card().unwrap().id, intent.id);
        let single = match f.db.get_work_item(&f.task).unwrap() {
            WorkItem::Chore(task) | WorkItem::Task(task) => task,
            other => panic!("expected a chore, got {other:?}"),
        };
        assert_eq!(single.merge_intent.unwrap().id, intent.id, "single-item reads agree");
    }

    #[test]
    fn a_request_without_a_head_uses_the_last_probed_head() {
        let f = fixture();
        let intent = f.db.record_merge_intent(&f.task, PR, None).unwrap();
        assert_eq!(intent.approved_head_sha.as_deref(), Some("head-1"));
    }

    #[test]
    fn asking_again_supersedes_the_standing_request() {
        let f = fixture();
        let first = f.request();
        let second = f.db.record_merge_intent(&f.task, PR, Some("head-2")).unwrap();
        assert_eq!(f.stored(&first.id).status, MERGE_INTENT_SUPERSEDED);
        assert_eq!(f.on_card().unwrap().id, second.id);
        assert_eq!(f.on_card().unwrap().approved_head_sha.as_deref(), Some("head-2"));
    }

    #[test]
    fn a_failed_merge_attempt_and_its_automatic_fix_leave_the_request_standing() {
        let f = fixture();
        let intent = f.request();
        // The submission is retired when the PR leaves the queue.
        f.db.record_github_merge_intent(
            GithubMergeIntentInsertInput::builder()
                .work_item_id(&f.task)
                .pr_url(PR)
                .head_sha("head-1")
                .build(),
            "{}",
        )
        .unwrap();
        f.db.connect()
            .unwrap()
            .execute("UPDATE github_merge_intents SET created_at = '100'", [])
            .unwrap();
        f.db.retire_github_merge_intent_on_dequeue(&f.task, PR, "head-1", 200)
            .unwrap()
            .unwrap();
        assert_eq!(
            f.db.note_merge_intent_left_queue(
                &f.task,
                PR,
                "FAILED_CHECKS",
                boss_engine_utils::epoch_time::now_epoch_secs()
            )
            .unwrap(),
            MergeQueueDeparture::Standing
        );
        // CI remediation blocks the card, spawns its revision and unblocks it.
        f.db.mark_chore_blocked_ci_failure(&f.task, PR, None).unwrap();
        assert_eq!(
            f.db.task_blocked_reason(&f.task).unwrap().as_deref(),
            Some("ci_failure")
        );
        f.revision(&format!("{CREATED_VIA_CI_FIX_PREFIX}attempt-1"));
        f.db.clear_chore_blocked_ci_failure(&f.task, PR).unwrap();
        f.db.mark_chore_blocked_ci_failure_exhausted(&f.task, PR).unwrap();
        assert_eq!(
            f.db.task_blocked_reason(&f.task).unwrap().as_deref(),
            Some("ci_failure_exhausted")
        );
        f.db.clear_chore_blocked_ci_failure(&f.task, PR).unwrap();
        // So does a conflict against the base.
        f.db.mark_chore_blocked_merge_conflict(&f.task, PR).unwrap();
        assert_eq!(
            f.db.task_blocked_reason(&f.task).unwrap().as_deref(),
            Some("merge_conflict")
        );
        f.revision(&format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}attempt-2"));
        f.db.clear_chore_blocked_merge_conflict(&f.task, PR).unwrap();
        assert_eq!(f.db.task_blocked_reason(&f.task).unwrap(), None);
        assert!(f.stored(&intent.id).is_active());
        assert_eq!(f.on_card().unwrap().id, intent.id);
    }

    #[test]
    fn a_fix_is_owed_from_a_failed_checks_departure_until_one_is_started() {
        let f = fixture();
        let intent = f.request();
        assert_eq!(
            f.db.merge_intent_remediation(&intent).unwrap(),
            MergeIntentRemediation::default()
        );
        f.db.note_merge_intent_left_queue(
            &f.task,
            PR,
            "failed_checks",
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .unwrap();
        assert!(f.db.merge_intent_remediation(&intent).unwrap().fix_owed);
        // The rebounce pass mints the attempt, as `ci_watch` does.
        f.db.insert_ci_remediation(
            CiRemediationInsertInput::builder()
                .product_id(&f.product)
                .work_item_id(&f.task)
                .pr_url(PR)
                .pr_number(42)
                .head_branch("feature")
                .head_sha_at_trigger("mq:head-1")
                .attempt_kind("fix")
                .consumes_budget(1)
                .failed_checks("[]")
                .failure_kind("merge_queue_rebounce")
                .before_commit_sha("queue-commit")
                .build(),
        )
        .unwrap()
        .unwrap();
        let remediation = f.db.merge_intent_remediation(&intent).unwrap();
        assert!(!remediation.fix_owed, "the fix that was owed has been started");
        assert_eq!(remediation.ci_fixes, 1);
        assert_eq!(remediation.conflict_resolutions, 0);
    }

    #[test]
    fn a_declined_fix_ends_the_request_when_automatic_maintenance_is_off() {
        let f = fixture();
        f.db.connect()
            .unwrap()
            .execute(
                "UPDATE products SET auto_pr_maintenance_enabled = 0 WHERE id = ?1",
                [&f.product],
            )
            .unwrap();
        let intent = f.request();
        assert_eq!(
            f.db.note_merge_intent_left_queue(
                &f.task,
                PR,
                "failed_checks",
                boss_engine_utils::epoch_time::now_epoch_secs()
            )
            .unwrap(),
            MergeQueueDeparture::Standing
        );
        assert!(
            f.db.decline_owed_merge_intent_ci_fix(&f.task, PR, MergeIntentCiFixDecline::MaintenanceDisabled)
                .unwrap()
        );
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_NEEDS_REVIEW);
        assert_eq!(ended.reason.as_deref(), Some(MERGE_INTENT_REASON_NO_DECISION));
        assert!(
            ended.detail.unwrap().contains("automatic PR maintenance is off"),
            "the card says why the merge will not be retried"
        );
        assert!(f.db.active_merge_intent(&f.task, PR).unwrap().is_none());
    }

    #[test]
    fn a_declined_fix_ends_the_request_when_the_ci_fix_budget_is_spent() {
        let f = fixture();
        f.db.set_ci_attempt_budget(&f.task, Some(0)).unwrap();
        let intent = f.request();
        assert_eq!(
            f.db.note_merge_intent_left_queue(
                &f.task,
                PR,
                "failed_checks",
                boss_engine_utils::epoch_time::now_epoch_secs()
            )
            .unwrap(),
            MergeQueueDeparture::Standing
        );
        assert!(
            f.db.decline_owed_merge_intent_ci_fix(&f.task, PR, MergeIntentCiFixDecline::BudgetExhausted)
                .unwrap()
        );
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_NEEDS_REVIEW);
        assert!(ended.detail.unwrap().contains("CI-fix budget is spent"));
        assert!(f.db.active_merge_intent(&f.task, PR).unwrap().is_none());
    }

    #[test]
    fn a_declined_fix_ends_the_request_when_the_same_head_already_had_a_ci_fix() {
        let f = fixture();
        f.request();
        f.db.insert_ci_remediation(
            CiRemediationInsertInput::builder()
                .product_id(&f.product)
                .work_item_id(&f.task)
                .pr_url(PR)
                .pr_number(42)
                .head_branch("feature")
                .head_sha_at_trigger("mq:head-1")
                .attempt_kind("fix")
                .consumes_budget(1)
                .failed_checks("[]")
                .failure_kind("merge_queue_rebounce")
                .before_commit_sha("queue-commit")
                .build(),
        )
        .unwrap()
        .unwrap();
        f.db.connect()
            .unwrap()
            .execute("UPDATE ci_remediations SET created_at = '1'", [])
            .unwrap();
        // A new merge request of the same head, then the queue rejects it
        // again. Rebounce will not mint a second attempt for this episode.
        let second = f.request();
        assert_eq!(
            f.db.note_merge_intent_left_queue(
                &f.task,
                PR,
                "failed_checks",
                boss_engine_utils::epoch_time::now_epoch_secs()
            )
            .unwrap(),
            MergeQueueDeparture::Standing
        );
        assert!(
            f.db.decline_owed_merge_intent_ci_fix(&f.task, PR, MergeIntentCiFixDecline::EpisodeAlreadyHandled)
                .unwrap()
        );
        let ended = f.stored(&second.id);
        assert_eq!(ended.status, MERGE_INTENT_NEEDS_REVIEW);
        assert!(ended.detail.unwrap().contains("already had a CI fix"));
        assert!(f.db.active_merge_intent(&f.task, PR).unwrap().is_none());
    }

    #[test]
    fn a_repeated_decline_after_the_fix_was_minted_leaves_the_request_standing() {
        let f = fixture();
        let intent = f.request();
        let event_time = boss_engine_utils::epoch_time::now_epoch_secs() - 1;
        // Rebounce mints the attempt for this dequeue.
        f.db.insert_ci_remediation(
            CiRemediationInsertInput::builder()
                .product_id(&f.product)
                .work_item_id(&f.task)
                .pr_url(PR)
                .pr_number(42)
                .head_branch("feature")
                .head_sha_at_trigger("mq:head-1")
                .attempt_kind("fix")
                .consumes_budget(1)
                .failed_checks("[]")
                .failure_kind("merge_queue_rebounce")
                .before_commit_sha("queue-commit")
                .build(),
        )
        .unwrap()
        .unwrap();
        // Observation lagged the mint: use the event time, not the later sweep time.
        f.db.note_merge_intent_left_queue(&f.task, PR, "failed_checks", event_time)
            .unwrap();
        let stamp: String =
            f.db.connect()
                .unwrap()
                .query_row(
                    "SELECT fix_owed_since FROM merge_intents WHERE id = ?1",
                    [&intent.id],
                    |row| row.get(0),
                )
                .unwrap();
        assert_eq!(stamp, event_time.to_string());
        // The next sweep sees the same episode, finds it handled, and says so.
        assert!(
            !f.db
                .decline_owed_merge_intent_ci_fix(&f.task, PR, MergeIntentCiFixDecline::EpisodeAlreadyHandled)
                .unwrap()
        );
        assert!(f.db.active_merge_intent(&f.task, PR).unwrap().is_some());
        assert!(!f.db.merge_intent_remediation(&intent).unwrap().fix_owed);
    }

    #[test]
    fn an_older_same_head_attempt_counts_as_handling_an_owed_fix() {
        let f = fixture();
        let intent = f.request();
        f.db.note_merge_intent_left_queue(
            &f.task,
            PR,
            "failed_checks",
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .unwrap();
        assert!(f.db.merge_intent_remediation(&intent).unwrap().fix_owed);
        let attempt =
            f.db.insert_ci_remediation(
                CiRemediationInsertInput::builder()
                    .product_id(&f.product)
                    .work_item_id(&f.task)
                    .pr_url(PR)
                    .pr_number(42)
                    .head_branch("feature")
                    .head_sha_at_trigger("mq:head-1")
                    .attempt_kind("fix")
                    .consumes_budget(1)
                    .failed_checks("[]")
                    .failure_kind("merge_queue_rebounce")
                    .before_commit_sha("queue-commit")
                    .build(),
            )
            .unwrap()
            .unwrap();
        // Clock skew / a row minted before the dequeue stamp still counts.
        f.db.connect()
            .unwrap()
            .execute(
                "UPDATE ci_remediations SET created_at = '1' WHERE id = ?1",
                [&attempt.id],
            )
            .unwrap();
        let remediation = f.db.merge_intent_remediation(&intent).unwrap();
        assert!(!remediation.fix_owed, "the same-head episode is already handled");
    }

    #[test]
    fn declining_an_owed_fix_from_the_rebounce_path_ends_the_request() {
        let f = fixture();
        let intent = f.request();
        f.db.note_merge_intent_left_queue(
            &f.task,
            PR,
            "failed_checks",
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .unwrap();
        assert!(f.db.merge_intent_remediation(&intent).unwrap().fix_owed);
        assert!(
            f.db.decline_owed_merge_intent_ci_fix(&f.task, PR, MergeIntentCiFixDecline::Suppressed)
                .unwrap()
        );
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_NEEDS_REVIEW);
        assert!(ended.detail.unwrap().contains("suppressed"));
        assert!(
            !f.db
                .decline_owed_merge_intent_ci_fix(&f.task, PR, MergeIntentCiFixDecline::Suppressed)
                .unwrap(),
            "already ended"
        );
    }

    #[test]
    fn a_later_revision_or_head_move_takes_an_ended_request_off_the_card() {
        let f = fixture();
        let intent = f.request();
        f.db.end_merge_intent(
            &intent.id,
            &MergeIntentEnd {
                status: MERGE_INTENT_NEEDS_REVIEW,
                reason: MERGE_INTENT_REASON_NO_DECISION,
                detail: Some("Review before merging again: an automatic CI fix changed the PR.".into()),
                decided_head_sha: Some("head-1".into()),
            },
        )
        .unwrap();
        assert_eq!(f.on_card().unwrap().id, intent.id);

        let f = fixture();
        let intent = f.request();
        f.db.end_merge_intent(
            &intent.id,
            &MergeIntentEnd {
                status: MERGE_INTENT_NEEDS_REVIEW,
                reason: MERGE_INTENT_REASON_NO_DECISION,
                detail: Some("The PR changed.".into()),
                decided_head_sha: Some("head-1".into()),
            },
        )
        .unwrap();
        f.revision(&format!("{CREATED_VIA_CI_FIX_PREFIX}later"));
        assert!(f.on_card().is_none(), "a new revision means the old sentence is stale");

        let f = fixture();
        let intent = f.request();
        f.db.end_merge_intent(
            &intent.id,
            &MergeIntentEnd {
                status: MERGE_INTENT_NEEDS_REVIEW,
                reason: MERGE_INTENT_REASON_NO_DECISION,
                detail: Some("The PR changed.".into()),
                decided_head_sha: Some("head-1".into()),
            },
        )
        .unwrap();
        f.db.connect()
            .unwrap()
            .execute("UPDATE tasks SET pr_head_sha = 'head-2' WHERE id = ?1", [&f.task])
            .unwrap();
        assert!(f.on_card().is_none(), "the PR head moved past the one the ending named");
    }

    #[test]
    fn leaving_the_queue_for_a_reason_the_engine_does_not_fix_ends_the_request() {
        for reason in ["manual_removal", "queue_cleared", "something_new"] {
            let f = fixture();
            let intent = f.request();
            assert_eq!(
                f.db.note_merge_intent_left_queue(&f.task, PR, reason, boss_engine_utils::epoch_time::now_epoch_secs())
                    .unwrap(),
                MergeQueueDeparture::Ended,
                "{reason}"
            );
            let ended = f.stored(&intent.id);
            assert_eq!(ended.status, MERGE_INTENT_LAPSED);
            assert_eq!(
                ended.reason.as_deref(),
                Some(MERGE_INTENT_REASON_REMOVED_FROM_MERGE_QUEUE)
            );
            assert!(ended.detail.unwrap().contains(reason));
            assert_eq!(f.on_card().unwrap().id, intent.id, "the card says why");
        }
        let f = fixture();
        f.request();
        assert_eq!(
            f.db.note_merge_intent_left_queue(
                &f.task,
                PR,
                "merge_conflict",
                boss_engine_utils::epoch_time::now_epoch_secs()
            )
            .unwrap(),
            MergeQueueDeparture::Standing
        );
        assert_eq!(
            f.db.note_merge_intent_left_queue(
                &f.task,
                "https://github.com/example/repo/pull/7",
                "manual_removal",
                boss_engine_utils::epoch_time::now_epoch_secs()
            )
            .unwrap(),
            MergeQueueDeparture::NoIntent
        );
    }

    #[test]
    fn dequeue_replays_cannot_replace_a_newer_obligation_or_request() {
        let f = fixture();
        let intent = f.request();
        f.db.connect()
            .unwrap()
            .execute("UPDATE merge_intents SET created_at = '10'", [])
            .unwrap();
        for time in [20, 20, 19, 10, 9] {
            f.db.note_merge_intent_left_queue(&f.task, PR, "failed_checks", time)
                .unwrap();
        }
        let stamp: String =
            f.db.connect()
                .unwrap()
                .query_row(
                    "SELECT fix_owed_since FROM merge_intents WHERE id = ?1",
                    [&intent.id],
                    |row| row.get(0),
                )
                .unwrap();
        assert_eq!(stamp, "20");
        f.db.note_merge_intent_left_queue(&f.task, PR, "manual_removal", 19)
            .unwrap();
        assert!(f.db.active_merge_intent(&f.task, PR).unwrap().is_some());
        let replacement = f.request();
        f.db.note_merge_intent_left_queue(&f.task, PR, "manual_removal", 20)
            .unwrap();
        assert_eq!(
            f.db.active_merge_intent(&f.task, PR).unwrap().unwrap().id,
            replacement.id
        );
    }

    #[test]
    fn upgrading_revision_triggers_keeps_the_new_ending_on_the_card() {
        let f = fixture();
        let intent = f.request();
        let conn = f.db.connect().unwrap();
        // Recreate the shipped trigger pair, then apply the new migration.
        migrate(&conn).unwrap();
        migrate_dequeue_tracking(&conn).unwrap();
        let clear_triggers: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name = 'merge_intents_clear_ended_on_revision'",
            [], |row| row.get(0)
        ).unwrap();
        assert_eq!(clear_triggers, 0);
        drop(conn);
        f.revision("operator");
        assert_eq!(f.on_card().unwrap().id, intent.id);
        assert_eq!(f.stored(&intent.id).status, MERGE_INTENT_INVALIDATED);
    }

    #[test]
    fn a_revision_that_is_not_an_automatic_fix_ends_the_request() {
        for created_via in [
            "pr_review:exec_1",
            "cli",
            "operator",
            "guide-comment:series_1",
            "progression:exec_2",
        ] {
            let f = fixture();
            let intent = f.request();
            f.revision(created_via);
            let ended = f.stored(&intent.id);
            assert_eq!(ended.status, MERGE_INTENT_INVALIDATED, "{created_via}");
            assert_eq!(
                ended.reason.as_deref(),
                Some(MERGE_INTENT_REASON_NON_REMEDIATION_REVISION)
            );
            assert_eq!(f.on_card().unwrap().id, intent.id, "the card says why");
        }
    }

    #[test]
    fn moving_the_card_or_rebinding_its_pr_withdraws_the_request() {
        let f = fixture();
        let intent = f.request();
        f.patch(WorkItemPatch {
            status: Some("active".into()),
            ..Default::default()
        });
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_WITHDRAWN);
        assert_eq!(ended.reason.as_deref(), Some(MERGE_INTENT_REASON_MOVED_OUT_OF_REVIEW));
        f.patch(WorkItemPatch {
            status: Some("in_review".into()),
            ..Default::default()
        });
        assert!(f.on_card().is_none(), "returning to review does not bring it back");
        assert!(f.db.active_merge_intent(&f.task, PR).unwrap().is_none());

        let intent = f.request();
        f.patch(WorkItemPatch {
            pr_url: Some("https://github.com/example/repo/pull/43".into()),
            ..Default::default()
        });
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_WITHDRAWN);
        assert_eq!(ended.reason.as_deref(), Some(MERGE_INTENT_REASON_PR_REBOUND));
        assert!(f.on_card().is_none());
    }

    #[test]
    fn parking_ends_the_request_for_the_item_and_for_a_parked_project() {
        let f = fixture();
        let intent = f.request();
        f.db.park_work_item_as_actor(&f.task, true, Some("later"), "human")
            .unwrap();
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_INVALIDATED);
        assert_eq!(ended.reason.as_deref(), Some(MERGE_INTENT_REASON_PARKED));
        f.db.park_work_item_as_actor(&f.task, false, None, "human").unwrap();
        assert!(
            f.db.active_merge_intent(&f.task, PR).unwrap().is_none(),
            "un-parking does not revive it"
        );
        assert_eq!(f.on_card().unwrap().id, intent.id, "the un-parked card says why");

        let db = WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap();
        let product = crate::test_support::create_test_product_named(&db, "Parked project").id;
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(&product)
                    .name("Later")
                    .no_design_task(true)
                    .build(),
            )
            .unwrap()
            .id;
        let task = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(&product)
                    .project_id(&project)
                    .name("Member")
                    .autostart(false)
                    .build(),
            )
            .unwrap()
            .id;
        db.update_work_item(
            &task,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(PR.into()),
                ..Default::default()
            },
        )
        .unwrap();
        let intent = db.record_merge_intent(&task, PR, Some("head-1")).unwrap();
        db.park_work_item_as_actor(&project, true, None, "human").unwrap();
        let ended = query_intent(&db.connect().unwrap(), "id = ?1", [&intent.id])
            .unwrap()
            .unwrap();
        assert_eq!(ended.reason.as_deref(), Some(MERGE_INTENT_REASON_PARKED));
    }

    #[test]
    fn requested_changes_and_unrelated_blocks_end_the_request() {
        let f = fixture();
        let intent = f.request();
        f.db.update_task_pr_poll_state(
            &f.task,
            PrPollStateInput {
                caller: "merge_intent_test",
                ci_required_state: "success",
                review_required_state: "changes_requested",
                pr_mergeable_state: "mergeable",
                pr_head_sha: Some("head-1"),
                ..Default::default()
            },
        )
        .unwrap();
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_INVALIDATED);
        assert_eq!(ended.reason.as_deref(), Some(MERGE_INTENT_REASON_CHANGES_REQUESTED));

        let f = fixture();
        let intent = f.request();
        f.patch(WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some("dependency".into()),
            ..Default::default()
        });
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_INVALIDATED);
        assert_eq!(ended.reason.as_deref(), Some(MERGE_INTENT_REASON_BLOCKED));
    }

    #[test]
    fn an_observed_merge_ends_the_request_as_merged_before_the_card_finishes() {
        let f = fixture();
        let intent = f.request();
        f.db.retire_github_merge_intent(&f.task, PR, "merged").unwrap();
        f.db.mark_chore_pr_merged(&f.task, PR).unwrap();
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_MERGED);
        assert_eq!(ended.reason.as_deref(), Some(MERGE_INTENT_REASON_MERGED));

        // A late probe of a PR the item is no longer bound to ends nothing.
        let f = fixture();
        let intent = f.request();
        f.db.retire_github_merge_intent(&f.task, "https://github.com/example/repo/pull/7", "merged")
            .unwrap();
        assert!(f.stored(&intent.id).is_active());

        // A card finished some other way still ends its request.
        let f = fixture();
        let intent = f.request();
        f.db.mark_chore_pr_closed_unmerged(&f.task, PR).unwrap();
        let ended = f.stored(&intent.id);
        assert_eq!(ended.status, MERGE_INTENT_CLOSED);
        assert_eq!(ended.reason.as_deref(), Some(MERGE_INTENT_REASON_TASK_FINISHED));
    }

    #[test]
    fn ending_by_id_never_touches_the_request_that_replaced_it() {
        let f = fixture();
        let first = f.request();
        let second = f.request();
        let end = MergeIntentEnd {
            status: MERGE_INTENT_NEEDS_REVIEW,
            reason: MERGE_INTENT_REASON_NO_DECISION,
            detail: Some("The PR changed.".into()),
            decided_head_sha: Some("head-2".into()),
        };
        assert!(!f.db.end_merge_intent(&first.id, &end).unwrap(), "already superseded");
        assert!(f.stored(&second.id).is_active());
        assert!(f.db.end_merge_intent(&second.id, &end).unwrap());
        let shown = f.on_card().unwrap();
        assert_eq!(shown.id, second.id);
        assert_eq!(shown.status, MERGE_INTENT_NEEDS_REVIEW);
        assert_eq!(shown.decided_head_sha.as_deref(), Some("head-2"));
        assert_eq!(shown.detail.as_deref(), Some("The PR changed."));
        assert!(shown.ended_at.is_some());
    }

    #[test]
    fn writes_to_the_table_are_journaled_as_board_input_changes() {
        let f = fixture();
        let conn = f.db.connect().unwrap();
        let journaled = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM board_changes WHERE source = 'merge_intents' AND item_id = ?1",
                [&f.task],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(journaled(&conn), 0);
        drop(conn);
        let intent = f.request();
        let conn = f.db.connect().unwrap();
        let after_request = journaled(&conn);
        assert!(after_request > 0);
        drop(conn);
        f.db.end_merge_intent(
            &intent.id,
            &MergeIntentEnd {
                status: MERGE_INTENT_LAPSED,
                reason: MERGE_INTENT_REASON_REMOVED_FROM_MERGE_QUEUE,
                detail: None,
                decided_head_sha: None,
            },
        )
        .unwrap();
        assert!(journaled(&f.db.connect().unwrap()) > after_request);
    }
}
