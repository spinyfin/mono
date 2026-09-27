//! Immutable starting points for incremental guide updates and revision attribution.
use super::*;

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pr_review_guide_updates (
            attempt_id TEXT PRIMARY KEY REFERENCES pr_review_guide_attempts(id) ON DELETE CASCADE,
            previous_version_id TEXT NOT NULL REFERENCES pr_review_guide_versions(id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS pr_review_guide_revision_heads (
            root_task_id TEXT NOT NULL,
            pr_url TEXT NOT NULL,
            head_sha TEXT NOT NULL,
            revision_task_id TEXT NOT NULL,
            observation_sequence INTEGER NOT NULL,
            PRIMARY KEY(root_task_id, pr_url, head_sha)
         );",
    )?;
    Ok(())
}

/// Snapshot the readable version in the admission transaction. Superseding a
/// queued update still starts from the published version, never unpublished prose.
pub(super) fn snapshot(conn: &Connection, attempt: &str, series: &str, comparison: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO pr_review_guide_updates (attempt_id, previous_version_id)
         SELECT ?1, v.id FROM pr_review_guide_source_series s
         JOIN pr_review_guide_versions v ON v.id = s.readable_version_id
         JOIN pr_review_guide_source_comparisons old ON old.id = v.comparison_id
         JOIN pr_review_guide_source_comparisons new ON new.id = ?3
         WHERE s.id = ?2 AND old.head_sha != new.head_sha",
        params![attempt, series, comparison],
    )?;
    conn.execute(
        "UPDATE pr_review_guide_attempts SET prompt_version = ?2
         WHERE id = ?1 AND EXISTS (SELECT 1 FROM pr_review_guide_updates WHERE attempt_id = ?1)",
        params![attempt, boss_review_guide::UPDATE_PROMPT_VERSION],
    )?;
    Ok(())
}

impl WorkDb {
    pub(crate) fn review_guide_comparison_has_attempt(&self, comparison: &str) -> Result<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM pr_review_guide_attempts WHERE comparison_id = ?1)",
            [comparison],
            |row| row.get(0),
        )?)
    }

    /// The old head and original Markdown are immutable and bound to this attempt.
    pub(crate) fn review_guide_update_context(&self, execution: &str) -> Result<Option<(String, String)>> {
        let conn = self.connect()?;
        let context = conn
            .query_row(
                "SELECT c.head_sha, v.markdown FROM pr_review_guide_updates u
             JOIN pr_review_guide_attempts a ON a.id = u.attempt_id
             JOIN pr_review_guide_versions v ON v.id = u.previous_version_id
             JOIN pr_review_guide_source_comparisons c ON c.id = v.comparison_id
             WHERE a.execution_id = ?1",
                [execution],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let requires_update: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pr_review_guide_attempts WHERE execution_id = ?1 AND prompt_version = ?2)",
            params![execution, boss_review_guide::UPDATE_PROMPT_VERSION],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            !requires_update || context.is_some(),
            "incremental guide starting version is missing"
        );
        Ok(context)
    }

    pub(crate) fn record_review_guide_revision_head(
        &self,
        root: &str,
        pr: &str,
        head: &str,
        revision: &str,
        sequence: i64,
    ) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO pr_review_guide_revision_heads
             (root_task_id, pr_url, head_sha, revision_task_id, observation_sequence)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_task_id, pr_url, head_sha) DO UPDATE SET
               revision_task_id = excluded.revision_task_id,
               observation_sequence = excluded.observation_sequence
             WHERE excluded.observation_sequence > observation_sequence",
            params![root, pr, head, revision, sequence],
        )?;
        Ok(())
    }
}

/// Shared by board reads and the series-scoped viewer poll. A revision label
/// belongs to an exact head, never to a replacement PR.
pub(in crate::work) fn status(conn: &Connection, root: &str, pr: &str) -> Result<Option<String>> {
    let row = conn
        .query_row(
            "SELECT s.guide_lifecycle, c.head_sha, t.id, t.short_id
         FROM pr_review_guide_source_series s
         JOIN pr_review_guide_source_comparisons c ON c.id = s.selected_comparison_id
         JOIN pr_review_guide_revision_heads r ON r.root_task_id = s.root_task_id
           AND r.pr_url = s.canonical_pr_url
           AND ((s.last_capture_error IS NULL AND r.head_sha = c.head_sha)
             OR (s.last_capture_error IS NOT NULL AND r.observation_sequence = s.latest_observation_sequence))
         JOIN tasks t ON t.id = r.revision_task_id
         WHERE s.root_task_id = ?1 AND s.canonical_pr_url = ?2
           AND s.readable_version_id IS NOT NULL
           AND (s.last_capture_error IS NOT NULL OR EXISTS (SELECT 1 FROM pr_review_guide_attempts a
             JOIN pr_review_guide_updates u ON u.attempt_id = a.id
             WHERE a.series_id = s.id AND a.request_epoch = s.request_epoch
               AND a.comparison_id = s.selected_comparison_id))
         ORDER BY r.observation_sequence DESC LIMIT 1",
            params![root, pr],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(|(lifecycle, head, id, short)| {
        let label = boss_protocol::short_id_label(short).unwrap_or(id);
        match lifecycle.as_str() {
            "queued" | "generating" => format!("Updating after revision {label}..."),
            "ready" => format!("Updated for revision {label} at {head}"),
            _ => format!("Update after revision {label} failed; previous guide retained"),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        create_active_chore, create_product, open_db, review_guide_source_packet, seed_review_guide_series,
    };

    #[test]
    fn retention_preserves_a_live_updates_starting_version_then_reclaims_terminal_history() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "guide retention");
        let (series, comparison) = seed_review_guide_series(&db, &root);
        let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
        let PublishReviewGuideOutcome::Published(old) = db
            .publish_pr_review_guide_version(&first.id, "# Original", "original")
            .unwrap()
        else {
            panic!("publish");
        };
        let PrSourceCapturePersistOutcome::Stored(new) = db
            .persist_pr_review_guide_source_capture(
                &root,
                2,
                PrSourceCaptureTrigger::Completion,
                &review_guide_source_packet("base", "new-head"),
            )
            .unwrap()
        else {
            panic!("capture");
        };
        let update = db
            .create_pr_review_guide_attempt(&series, &new.comparison_id, "test")
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&root])
            .unwrap();
        conn.execute("UPDATE pr_review_guide_versions SET generated_at = '1'", [])
            .unwrap();
        conn.execute("UPDATE pr_review_guide_source_comparisons SET captured_at = '1'", [])
            .unwrap();
        conn.execute("UPDATE pr_review_guide_attempts SET created_at = '1'", [])
            .unwrap();
        drop(conn);
        db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
        assert!(db.get_pr_review_guide_version(&old.id).unwrap().is_some());
        assert_eq!(
            db.get_pr_review_guide_summary_for_root(&root)
                .unwrap()
                .unwrap()
                .readable_version_id,
            Some(old.id.clone())
        );
        db.fail_pr_review_guide_attempt(&update.id, "failed").unwrap();
        db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
        assert!(db.get_pr_review_guide_version(&old.id).unwrap().is_none());
    }

    #[test]
    fn failed_source_capture_keeps_published_guide_and_fences_old_jobs() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "capture failure");
        let (series, comparison) = seed_review_guide_series(&db, &root);
        let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
        let PublishReviewGuideOutcome::Published(old) = db
            .publish_pr_review_guide_version(&first.id, "# Original", "original")
            .unwrap()
        else {
            panic!("publish");
        };
        let stale = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
        let url = db
            .get_pr_review_guide_summary_for_root(&root)
            .unwrap()
            .unwrap()
            .canonical_pr_url;
        db.record_pr_review_guide_source_capture_failure(&root, &url, 10, "pinned delta unavailable")
            .unwrap();
        assert_eq!(
            db.publish_pr_review_guide_version(&stale.id, "# Late", "late").unwrap(),
            PublishReviewGuideOutcome::Superseded
        );
        let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
        assert_eq!(summary.lifecycle, "failed");
        assert_eq!(summary.readable_version_id, Some(old.id));
        assert!(
            db.retry_pr_review_guide(&root, None, "test")
                .unwrap_err()
                .to_string()
                .contains("recapture")
        );
    }
}
