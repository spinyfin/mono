//! `github_merge_intents` DAO: the durable record that a GitHub-native
//! `gh pr merge --auto --squash` succeeded for one precise PR head.
//!
//! GitHub's merge-queue and auto-merge projections are asynchronously
//! materialized. A just-successful command therefore outranks an immediately
//! empty probe until the poller observes an actual dequeue or terminal merge.

use super::*;

/// One `github_merge_intents` row.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct GithubMergeIntent {
    pub id: String,
    pub work_item_id: String,
    pub pr_url: String,
    pub head_sha: String,
    /// `active` while GitHub has not supplied terminal/dequeue evidence;
    /// `merged`, `closed`, or `dequeued` after that evidence arrives.
    pub status: String,
    pub created_at: String,
}

/// The exact GitHub-native merge request that succeeded.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct GithubMergeIntentInsertInput {
    pub work_item_id: String,
    pub pr_url: String,
    pub head_sha: String,
}

/// Whether recording a successful request changed the Merging projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GithubMergeIntentRecordOutcome {
    pub inserted: bool,
    pub merge_queue_state_changed: bool,
}

const GITHUB_MERGE_INTENT_COLUMNS: &str = "id, work_item_id, pr_url, head_sha, status, created_at";

fn map_github_merge_intent(row: &Row<'_>) -> rusqlite::Result<GithubMergeIntent> {
    Ok(GithubMergeIntent {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        pr_url: row.get(2)?,
        head_sha: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
    })
}

impl WorkDb {
    /// Atomically record a successful GitHub-native merge request and place
    /// its card in Merging. A duplicate for the same PR/head is harmless;
    /// one for a different live PR/head is an error rather than a false
    /// success for a request that was never recorded.
    pub fn record_github_merge_intent(
        &self,
        input: GithubMergeIntentInsertInput,
        merge_queue_detail: &str,
    ) -> Result<GithubMergeIntentRecordOutcome> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let id = next_id("gmi");
        let now = now_string();
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO github_merge_intents
                (id, work_item_id, pr_url, head_sha, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5)",
            params![id, input.work_item_id, input.pr_url, input.head_sha, now],
        )? > 0;

        if !inserted {
            let active = query_active_github_merge_intent(&tx, &input.work_item_id)?;
            match active {
                Some(active) if active.pr_url == input.pr_url && active.head_sha == input.head_sha => {}
                Some(active) => {
                    bail!(
                        "a GitHub merge intent is already active for {} at {}; cannot record {} at {}",
                        active.pr_url,
                        active.head_sha,
                        input.pr_url,
                        input.head_sha,
                    );
                }
                None => bail!(
                    "GitHub merge intent insert was ignored but no active intent was found for {}",
                    input.work_item_id,
                ),
            }
        }

        let changed = tx.execute(
            "UPDATE tasks
             SET merge_queue_state = 'queued', merge_queue_detail = ?2
             WHERE id = ?1
               AND deleted_at IS NULL
               AND (merge_queue_state IS NOT 'queued' OR merge_queue_detail IS NOT ?2)",
            params![input.work_item_id, merge_queue_detail],
        )? > 0;
        tx.commit()?;
        Ok(GithubMergeIntentRecordOutcome {
            inserted,
            merge_queue_state_changed: changed,
        })
    }

    /// The active GitHub-native intent for this exact task and PR.
    pub fn get_active_github_merge_intent(
        &self,
        work_item_id: &str,
        pr_url: &str,
    ) -> Result<Option<GithubMergeIntent>> {
        let conn = self.connect()?;
        query_active_github_merge_intent_for_pr(&conn, work_item_id, pr_url)
    }

    /// Whether an empty GitHub queue/auto-merge observation must preserve the
    /// prior Merging projection. The caller deliberately checks this only for
    /// an empty observation: a positive observation remains the newer,
    /// detailed projection.
    pub fn has_active_github_merge_intent(&self, work_item_id: &str, pr_url: &str) -> Result<bool> {
        Ok(self.get_active_github_merge_intent(work_item_id, pr_url)?.is_some())
    }

    /// Retire an active intent when the PR itself reaches a terminal state.
    /// The task terminal transition owns its queue-column clear; this records
    /// the corresponding durable fact so a future reopen/candidate cannot
    /// inherit a stale intent.
    pub fn retire_github_merge_intent(&self, work_item_id: &str, pr_url: &str, status: &str) -> Result<bool> {
        let conn = self.connect()?;
        let rows = conn.execute(
            "UPDATE github_merge_intents
             SET status = ?3
             WHERE work_item_id = ?1 AND pr_url = ?2 AND status = 'active'",
            params![work_item_id, pr_url, status],
        )?;
        Ok(rows > 0)
    }

    /// Retire a precise intent and clear its card only when GitHub's timeline
    /// observed a dequeue for the same PR head. Absence is deliberately not a
    /// departure signal: immediate post-submit probes routinely have neither
    /// queue nor auto-merge fields populated yet.
    pub fn retire_github_merge_intent_on_dequeue(
        &self,
        work_item_id: &str,
        pr_url: &str,
        head_sha: &str,
        event_created_at: &str,
    ) -> Result<Option<Option<String>>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let retired = tx.execute(
            "UPDATE github_merge_intents
             SET status = 'dequeued'
             WHERE work_item_id = ?1
               AND pr_url = ?2
               AND head_sha = ?3
               AND status = 'active'
               AND created_at < ?4",
            params![work_item_id, pr_url, head_sha, event_created_at],
        )?;
        if retired == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let prior_merge_queue_state = tx
            .query_row(
                "SELECT merge_queue_state FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        tx.execute(
            "UPDATE tasks
             SET merge_queue_state = NULL, merge_queue_detail = NULL
             WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id],
        )?;
        tx.commit()?;
        Ok(Some(prior_merge_queue_state))
    }
}

fn query_active_github_merge_intent(conn: &Connection, work_item_id: &str) -> Result<Option<GithubMergeIntent>> {
    let sql = format!(
        "SELECT {GITHUB_MERGE_INTENT_COLUMNS} FROM github_merge_intents \
         WHERE work_item_id = ?1 AND status = 'active'"
    );
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt
        .query_row(params![work_item_id], map_github_merge_intent)
        .optional()?)
}

fn query_active_github_merge_intent_for_pr(
    conn: &Connection,
    work_item_id: &str,
    pr_url: &str,
) -> Result<Option<GithubMergeIntent>> {
    let sql = format!(
        "SELECT {GITHUB_MERGE_INTENT_COLUMNS} FROM github_merge_intents \
         WHERE work_item_id = ?1 AND pr_url = ?2 AND status = 'active'"
    );
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt
        .query_row(params![work_item_id, pr_url], map_github_merge_intent)
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

    fn seed_task(db: &WorkDb) -> String {
        let product = crate::test_support::create_test_product_named(db, "GitHub intent product");
        let task = crate::test_support::create_test_chore_manual(db, product.id, "GitHub intent task");
        task.id
    }

    #[test]
    fn record_preserves_one_active_intent_and_moves_card_to_merging() {
        let db = test_db();
        let task = seed_task(&db);
        let detail = r#"{\"position\":null,\"state\":null,\"enqueued_at\":null,\"section_order\":500000}"#;
        let first = db
            .record_github_merge_intent(
                GithubMergeIntentInsertInput::builder()
                    .work_item_id(task.clone())
                    .pr_url("https://github.com/acme/widgets/pull/1")
                    .head_sha("head-1")
                    .build(),
                detail,
            )
            .unwrap();
        assert!(first.inserted);
        assert!(first.merge_queue_state_changed);
        assert!(
            db.has_active_github_merge_intent(&task, "https://github.com/acme/widgets/pull/1")
                .unwrap()
        );

        let duplicate = db
            .record_github_merge_intent(
                GithubMergeIntentInsertInput::builder()
                    .work_item_id(task.clone())
                    .pr_url("https://github.com/acme/widgets/pull/1")
                    .head_sha("head-1")
                    .build(),
                detail,
            )
            .unwrap();
        assert!(!duplicate.inserted);
        assert!(!duplicate.merge_queue_state_changed);
    }

    #[test]
    fn dequeue_requires_the_intent_head_and_clears_the_lane() {
        let db = test_db();
        let task = seed_task(&db);
        db.record_github_merge_intent(
            GithubMergeIntentInsertInput::builder()
                .work_item_id(task.clone())
                .pr_url("https://github.com/acme/widgets/pull/1")
                .head_sha("head-1")
                .build(),
            "{}",
        )
        .unwrap();

        assert!(
            db.retire_github_merge_intent_on_dequeue(
                &task,
                "https://github.com/acme/widgets/pull/1",
                "other-head",
                "9999999999",
            )
            .unwrap()
            .is_none()
        );
        assert!(
            db.has_active_github_merge_intent(&task, "https://github.com/acme/widgets/pull/1")
                .unwrap()
        );

        assert_eq!(
            db.retire_github_merge_intent_on_dequeue(
                &task,
                "https://github.com/acme/widgets/pull/1",
                "head-1",
                "9999999999",
            )
            .unwrap(),
            Some(Some("queued".to_owned()))
        );
        assert!(
            !db.has_active_github_merge_intent(&task, "https://github.com/acme/widgets/pull/1")
                .unwrap()
        );
    }
}
