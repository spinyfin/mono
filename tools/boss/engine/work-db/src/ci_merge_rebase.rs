//! Durable tracking for a Merge click on a `blocked: ci_failure` /
//! `blocked: ci_failure_exhausted` card whose branch was behind its base.
//!
//! A Merge click on such a card rebases the PR branch onto its base via the
//! existing mechanical rebase rung and arms the merge (`gh pr merge --auto
//! --squash` or Trunk merge queue). This table
//! records that one-shot rebase so the merge poller can:
//!
//! - ignore stale failing probes that still describe the pre-rebase head;
//! - clear the record when CI on the rebased head turns green or the PR ends;
//! - disarm the armed merge, return the card to normal CI-failure handling,
//!   and surface `"rebased onto <base>; CI still failing"` if CI on the
//!   rebased head is still red — consuming the row so one Merge click triggers
//!   at most one rebase (no loops).

use super::*;

pub(crate) const MIGRATION_ID: &str = "20261123_1625_ci_merge_rebases";

/// `merge_queue_detail.source` value for a Merge-click CI rebase whose rebased
/// head still failed CI. Preserved by `update_task_pr_poll_state` and handled
/// by the disarm recovery helpers alongside `conflict_resolution`.
pub(crate) const CI_MERGE_REBASE_DETAIL_SOURCE: &str = "ci_merge_rebase";

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ci_merge_rebases (
            work_item_id     TEXT PRIMARY KEY,
            pr_url           TEXT NOT NULL,
            base_branch      TEXT NOT NULL,
            head_sha_before  TEXT,
            rebased_head_sha TEXT,
            created_at       TEXT NOT NULL
         );",
    )?;
    Ok(())
}

/// Durable record of a mechanical rebase triggered by a Merge click on a
/// CI-blocked card.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CiMergeRebase {
    pub work_item_id: String,
    pub pr_url: String,
    pub base_branch: String,
    pub head_sha_before: Option<String>,
    pub rebased_head_sha: Option<String>,
    pub created_at: String,
}

pub(crate) fn clear_in_tx(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(conn.execute("DELETE FROM ci_merge_rebases WHERE work_item_id = ?1", [work_item_id])? > 0)
}

/// Clear a stopped-merge detail written by a failed post-rebase CI run once CI
/// clears or the card moves on. Never touches a live queue state.
pub(crate) fn clear_stopped_detail_in_tx(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE tasks SET merge_queue_detail = NULL
          WHERE id = ?1 AND merge_queue_state IS NULL
            AND CASE WHEN json_valid(merge_queue_detail)
                THEN json_extract(merge_queue_detail, '$.source') = ?2 ELSE 0 END",
        params![work_item_id, CI_MERGE_REBASE_DETAIL_SOURCE],
    )? > 0)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Record that a Merge click mechanically rebased `work_item_id`'s PR onto
    /// `base_branch` and armed its merge. Overwrites any older record for the
    /// same work item.
    pub fn record_ci_merge_rebase(
        &self,
        work_item_id: &str,
        pr_url: &str,
        base_branch: &str,
        head_sha_before: Option<&str>,
        rebased_head_sha: Option<&str>,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO ci_merge_rebases
                (work_item_id, pr_url, base_branch, head_sha_before, rebased_head_sha, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                work_item_id,
                pr_url,
                base_branch,
                head_sha_before,
                rebased_head_sha,
                now_string(),
            ],
        )?;
        Ok(())
    }

    /// Look up the pending post-rebase record for `(work_item_id, pr_url)`.
    pub fn get_ci_merge_rebase(&self, work_item_id: &str, pr_url: &str) -> Result<Option<CiMergeRebase>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT work_item_id, pr_url, base_branch, head_sha_before, rebased_head_sha, created_at
               FROM ci_merge_rebases
              WHERE work_item_id = ?1 AND pr_url = ?2",
            params![work_item_id, pr_url],
            |row| {
                Ok(CiMergeRebase {
                    work_item_id: row.get(0)?,
                    pr_url: row.get(1)?,
                    base_branch: row.get(2)?,
                    head_sha_before: row.get(3)?,
                    rebased_head_sha: row.get(4)?,
                    created_at: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Delete any pending post-rebase record for `work_item_id`.
    pub fn clear_ci_merge_rebase(&self, work_item_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        clear_in_tx(&conn, work_item_id)
    }

    /// Atomically consume a pending CI-merge rebase after CI on the rebased
    /// head came back red:
    ///
    /// 1. Deletes the `ci_merge_rebases` row (one-shot; prevents rebase loops).
    /// 2. Transitions any active GitHub merge intent for `(work_item_id, pr_url)`
    ///    to `disarm_pending` so the merge poller turns off auto-merge.
    /// 3. Marks any active Trunk merge intent `boss:withdraw` so the queue
    ///    poller calls `cancelPullRequest` and retires the intent only after
    ///    Trunk confirms — the card must not stay in Trunk's queue untracked.
    /// 4. Clears `merge_queue_state` and records a visible paused-merge detail
    ///    carrying `reason`.
    /// 5. If `clear_pre_rebase_block` is true and the task is still in a stale
    ///    pre-rebase `blocked: ci_failure` / `ci_failure_exhausted` state,
    ///    transitions it back to `in_review` so `ci_watch::on_ci_failure_detected`
    ///    can immediately re-enter its normal CI-failure handling for the new
    ///    head.
    ///
    /// Returns `true` when a pending record was consumed.
    pub fn stop_ci_merge_rebase_for_failure(
        &self,
        work_item_id: &str,
        pr_url: &str,
        reason: &str,
        clear_pre_rebase_block: bool,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let consumed = tx.execute(
            "DELETE FROM ci_merge_rebases WHERE work_item_id = ?1 AND pr_url = ?2",
            params![work_item_id, pr_url],
        )? > 0;
        if !consumed {
            tx.commit()?;
            return Ok(false);
        }
        crate::merge_conflict_merging::forget_confirmed_for_intents(&tx, work_item_id, "active")?;
        tx.execute(
            "UPDATE github_merge_intents SET status = ?3
              WHERE work_item_id = ?1 AND pr_url = ?2 AND status = 'active'",
            params![work_item_id, pr_url, crate::merge_conflict_merging::DISARM_PENDING],
        )?;
        crate::trunk_merge_intents::mark_active_trunk_intent_withdraw_in_tx(&tx, work_item_id)?;
        let paused = format!("Merge paused: {reason}");
        let detail = serde_json::json!({
            "source": CI_MERGE_REBASE_DETAIL_SOURCE,
            "status_label": paused,
            "paused_label": paused,
        })
        .to_string();
        tx.execute(
            "UPDATE tasks
                SET merge_queue_state = NULL,
                    merge_queue_detail = ?2,
                    blocked_detail = CASE
                        WHEN status = 'blocked' AND blocked_reason IN ('ci_failure', 'ci_failure_exhausted')
                        THEN ?3
                        ELSE blocked_detail
                    END
              WHERE id = ?1 AND pr_url = ?4 AND deleted_at IS NULL",
            params![work_item_id, detail, reason, pr_url],
        )?;
        if clear_pre_rebase_block {
            crate::blocking::clear_blocked_ci_failure_in_tx(&tx, work_item_id, pr_url, false, "engine")?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Stamp `blocked_detail` on a `blocked: ci_failure` /
    /// `blocked: ci_failure_exhausted` task (e.g. `"rebased onto main; CI still
    /// failing"`). No-op if the task is not blocked on CI failure.
    pub fn set_ci_failure_blocked_detail(&self, work_item_id: &str, pr_url: &str, detail: &str) -> Result<bool> {
        let conn = self.connect()?;
        Ok(conn.execute(
            "UPDATE tasks
                SET blocked_detail = ?3,
                    updated_at     = ?4
              WHERE id = ?1
                AND pr_url = ?2
                AND status = 'blocked'
                AND blocked_reason IN ('ci_failure', 'ci_failure_exhausted')
                AND deleted_at IS NULL",
            params![work_item_id, pr_url, detail, now_string()],
        )? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product};

    const PR: &str = "https://github.com/example/repo/pull/77";

    #[test]
    fn record_get_and_stop_ci_merge_rebase_is_one_shot_and_marks_disarm() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id, "Rebase CI");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(PR.into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.mark_chore_blocked_ci_failure_exhausted(&chore.id, PR).unwrap();
        db.record_github_merge_intent(
            GithubMergeIntentInsertInput::builder()
                .work_item_id(&chore.id)
                .pr_url(PR)
                .head_sha("head-after")
                .build(),
            "{}",
        )
        .unwrap();
        db.record_ci_merge_rebase(&chore.id, PR, "main", Some("head-before"), Some("head-after"))
            .unwrap();

        let stored = db
            .get_ci_merge_rebase(&chore.id, PR)
            .unwrap()
            .expect("record must persist");
        assert_eq!(stored.base_branch, "main");
        assert_eq!(stored.head_sha_before.as_deref(), Some("head-before"));
        assert_eq!(stored.rebased_head_sha.as_deref(), Some("head-after"));

        let consumed = db
            .stop_ci_merge_rebase_for_failure(&chore.id, PR, "rebased onto main; CI still failing", true)
            .unwrap();
        assert!(consumed);
        assert!(db.get_ci_merge_rebase(&chore.id, PR).unwrap().is_none());
        assert!(db.merge_disarm_pending(&chore.id, PR).unwrap());

        // Second call is a no-op (one-shot).
        let again = db
            .stop_ci_merge_rebase_for_failure(&chore.id, PR, "rebased onto main; CI still failing", true)
            .unwrap();
        assert!(!again);

        let WorkItem::Chore(task) = db.get_work_item(&chore.id).unwrap() else {
            panic!("expected chore");
        };
        assert_eq!(task.status, TaskStatus::InReview);
        assert!(task.merge_queue_state.is_none());
        assert!(
            task.merge_queue_detail
                .as_deref()
                .unwrap_or("")
                .contains("rebased onto main; CI still failing")
        );
    }

    #[test]
    fn stop_ci_merge_rebase_marks_active_trunk_intent_withdraw_instead_of_cancelling() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id, "Rebase CI trunk");
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(PR.into()),
                ..Default::default()
            },
        )
        .unwrap();
        db.mark_chore_blocked_ci_failure(&chore.id, PR, None).unwrap();
        let intent = db
            .insert_trunk_merge_intent(
                TrunkMergeIntentInsertInput::builder()
                    .work_item_id(&chore.id)
                    .pr_url(PR)
                    .pr_number(77)
                    .repo("example/repo")
                    .target_branch("main")
                    .build(),
            )
            .unwrap()
            .expect("intent inserted");
        db.record_ci_merge_rebase(&chore.id, PR, "main", Some("head-before"), Some("head-after"))
            .unwrap();

        assert!(
            db.stop_ci_merge_rebase_for_failure(&chore.id, PR, "rebased onto main; CI still failing", true)
                .unwrap()
        );

        let still_active = db
            .get_active_trunk_merge_intent(&chore.id)
            .unwrap()
            .expect("intent must stay active until Trunk confirms cancel");
        assert_eq!(still_active.id, intent.id);
        assert_eq!(still_active.status, "active");
        assert_eq!(
            still_active.last_trunk_state.as_deref(),
            Some(crate::TRUNK_INTENT_WITHDRAW)
        );
    }
}
