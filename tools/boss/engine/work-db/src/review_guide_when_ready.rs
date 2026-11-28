//! Storage reads behind the `review_guide_when_ready` experiment: whether a
//! PR family is ready for a person, and whether its selected comparison has
//! been given a guide yet.
//!
//! Both are cheap, indexed reads so the merge poller can ask them every pass
//! for a PR it has just observed with green required checks.

use super::*;

/// Executions of kind `pr_review` in these statuses are a review still in
/// progress for the family (the legacy single-reviewer path, which has no
/// review batch row).
const LIVE_EXECUTION_STATUSES: &str = "'queued', 'ready', 'waiting_dependency', 'claimed', 'running', 'waiting_human'";

impl WorkDb {
    /// Whether the PR family rooted at `root_task_id` is quiet: no pre-merge
    /// review batch is live, no review execution is live, and no revision in
    /// its chain is a live writer (see [`crate::live_chain_writer_sql`]).
    ///
    /// A quiet family with green required checks is what the experiment calls
    /// "ready for a person". Any of the three can still produce a new push, so
    /// a guide generated before they settle would be regenerated at once.
    pub fn review_guide_family_is_quiet(&self, root_task_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let cycle_root = chain_helpers::chain_root(&conn, root_task_id)?;
        if review_batches::has_live_pre_merge_review_batch_in(&conn, &cycle_root)? {
            return Ok(false);
        }
        let live_review: bool = conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM work_executions
                  WHERE work_item_id = ?1 AND kind = 'pr_review'
                    AND status IN ({LIVE_EXECUTION_STATUSES}))"
            ),
            [&cycle_root],
            |row| row.get(0),
        )?;
        if live_review {
            return Ok(false);
        }
        for revision_id in chain_helpers::collect_chain_revision_ids(&conn, &cycle_root)? {
            if is_live_chain_writer(&conn, &revision_id)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The selected comparison of `root_task_id`'s guide series for `pr_url`
    /// when it is at `head_sha` and has never been given a guide: no attempt of
    /// any status was ever made for it and it is not the readable guide's
    /// comparison. `None` when the series has not captured this head yet, or
    /// when the comparison has been tried, including after a failure.
    ///
    /// Failed and cancelled attempts count as tried on purpose. The poller
    /// asks on every pass, so a guide that fails must wait for a person to
    /// retry it rather than being relaunched each minute.
    pub fn review_guide_untried_comparison_at_head(
        &self,
        root_task_id: &str,
        pr_url: &str,
        head_sha: &str,
    ) -> Result<Option<(String, String)>> {
        let conn = self.connect()?;
        Ok(conn
            .query_row(
                "SELECT s.id, c.id
                 FROM pr_review_guide_source_series s
                 JOIN pr_review_guide_source_comparisons c ON c.id = s.selected_comparison_id
                 WHERE s.root_task_id = ?1 AND s.canonical_pr_url = ?2 AND c.head_sha = ?3
                   AND NOT EXISTS (SELECT 1 FROM pr_review_guide_attempts a WHERE a.comparison_id = c.id)
                   AND NOT EXISTS (SELECT 1 FROM pr_review_guide_versions v WHERE v.comparison_id = c.id)",
                params![root_task_id, pr_url, head_sha],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Whether the guide series of `root_task_id` has a readable guide for its
    /// currently selected comparison. `false` when there is no series, no
    /// readable guide, or the readable guide is for an earlier comparison.
    pub fn review_guide_is_current(&self, root_task_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        Ok(conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pr_review_guide_source_series s
                 JOIN pr_review_guide_versions v ON v.id = s.readable_version_id
                 WHERE s.id = (SELECT id FROM pr_review_guide_source_series
                     WHERE root_task_id = ?1
                     ORDER BY latest_observation_sequence DESC, id DESC LIMIT 1)
                   AND v.comparison_id = s.selected_comparison_id)",
            [root_task_id],
            |row| row.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        create_active_chore, create_product, seed_published_guide, seed_review_guide_series_for_pr,
    };

    const PR_URL: &str = "https://github.com/acme/widget/pull/9";

    fn root(db: &WorkDb) -> String {
        create_active_chore(db, &create_product(db), "impl")
    }

    #[test]
    fn a_family_with_nothing_running_is_quiet() {
        let (_dir, db) = crate::test_support::open_db();
        assert!(db.review_guide_family_is_quiet(&root(&db)).unwrap());
    }

    fn exec(db: &WorkDb, sql: &str, params: impl rusqlite::Params) {
        db.connect().unwrap().execute(sql, params).unwrap();
    }

    #[test]
    fn a_live_review_execution_or_revision_writer_makes_the_family_busy() {
        let (_dir, db) = crate::test_support::open_db();
        let root = root(&db);
        exec(
            &db,
            "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at)
             VALUES ('review-1', ?1, 'pr_review', 'running', 'https://github.com/acme/widget.git', 'now')",
            [&root],
        );
        assert!(!db.review_guide_family_is_quiet(&root).unwrap());
        exec(
            &db,
            "UPDATE work_executions SET status = 'completed' WHERE id = 'review-1'",
            [],
        );
        assert!(db.review_guide_family_is_quiet(&root).unwrap());

        let product: String = db
            .connect()
            .unwrap()
            .query_row("SELECT product_id FROM tasks WHERE id = ?1", [&root], |row| row.get(0))
            .unwrap();
        exec(
            &db,
            "INSERT INTO tasks (id, product_id, kind, name, status, parent_task_id, created_at, updated_at)
             VALUES ('rev-1', ?1, 'revision', 'fix', 'active', ?2, 'now', 'now')",
            params![product, root],
        );
        assert!(
            !db.review_guide_family_is_quiet(&root).unwrap(),
            "an active revision writes"
        );
        exec(&db, "UPDATE tasks SET status = 'in_review' WHERE id = 'rev-1'", []);
        assert!(
            db.review_guide_family_is_quiet(&root).unwrap(),
            "a finished revision does not"
        );
    }

    #[test]
    fn a_comparison_is_untried_until_any_attempt_or_guide_exists_for_it() {
        let (_dir, db) = crate::test_support::open_db();
        let root = root(&db);
        let (series, comparison) = seed_review_guide_series_for_pr(&db, &root, PR_URL, "base", "head");
        assert_eq!(
            db.review_guide_untried_comparison_at_head(&root, PR_URL, "head")
                .unwrap(),
            Some((series.clone(), comparison.clone()))
        );
        assert_eq!(
            db.review_guide_untried_comparison_at_head(&root, PR_URL, "other-head")
                .unwrap(),
            None,
            "a probe for a head that was not captured is not ready"
        );
        let attempt = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
        assert_eq!(
            db.review_guide_untried_comparison_at_head(&root, PR_URL, "head")
                .unwrap(),
            None
        );
        db.fail_pr_review_guide_attempt(&attempt.id, "boom").unwrap();
        assert_eq!(
            db.review_guide_untried_comparison_at_head(&root, PR_URL, "head")
                .unwrap(),
            None,
            "a failed attempt is not relaunched by the poller"
        );
    }

    #[test]
    fn a_published_guide_is_current_only_for_its_selected_comparison() {
        let (_dir, db) = crate::test_support::open_db();
        let (root, _) = seed_published_guide(&db, 9);
        assert!(db.review_guide_is_current(&root).unwrap());
        assert!(!db.review_guide_is_current("no-such-root").unwrap());
        let mut replacement = crate::test_support::review_guide_source_packet("base", "replacement-head");
        replacement.canonical_pr_url = "https://github.com/acme/widget/pull/10".to_owned();
        replacement.pr_number = 10;
        db.persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Creation, &replacement)
            .unwrap();
        assert!(
            !db.review_guide_is_current(&root).unwrap(),
            "a retired PR must not satisfy the latest series"
        );
    }
}
