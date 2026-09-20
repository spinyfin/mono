//! Retention and cross-process publication fencing for source packets.
use super::*;
use fs4::fs_std::FileExt;

pub(super) struct SourceRetentionPolicy {
    pub terminal_age_seconds: i64,
    pub recent_comparisons: i64,
}

impl Default for SourceRetentionPolicy {
    fn default() -> Self {
        Self {
            terminal_age_seconds: 30 * 24 * 60 * 60,
            recent_comparisons: 5,
        }
    }
}

/// All callers acquire the store lock before the DB connection. Publishers
/// share it; GC tries exclusively and skips instead of delaying publication.
/// Dropping the file releases the OS lock, including on process exit.
pub(super) fn packet_store_lock(root: &Path, exclusive: bool) -> Result<Option<fs::File>> {
    let directory = root.join(PACKET_ARTIFACT_DIR);
    fs::create_dir_all(&directory)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("publication.lock"))?;
    if exclusive {
        if !FileExt::try_lock_exclusive(&file)? {
            return Ok(None);
        }
    } else {
        FileExt::lock_shared(&file)?;
    }
    Ok(Some(file))
}

impl WorkDb {
    pub(super) fn prune_pr_review_guide_sources(&self, policy: SourceRetentionPolicy) -> Result<()> {
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let cutoff = now - policy.terminal_age_seconds;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Terminal, aged series: drop published guide versions that no
        // `work_comments` row still cites, then their attempts, so the
        // comparison DELETE below can reclaim the packet blob. Versions that
        // still have comments keep the comparison (RESTRICT) and therefore
        // the on-disk packet. Active series are untouched here.
        tx.execute(
            "UPDATE pr_review_guide_source_series SET readable_version_id = NULL
             WHERE readable_version_id IN (
                SELECT v.id FROM pr_review_guide_versions v
                JOIN pr_review_guide_source_comparisons c ON c.id = v.comparison_id
                JOIN pr_review_guide_source_series s ON s.id = v.series_id
                LEFT JOIN tasks t ON t.id = s.root_task_id
                WHERE (t.id IS NULL OR t.deleted_at IS NOT NULL OR t.status IN ('done', 'archived'))
                  AND CAST(c.captured_at AS INTEGER) < ?1
                  AND NOT EXISTS (SELECT 1 FROM work_comments w WHERE w.guide_version_id = v.id)
             )",
            [cutoff],
        )?;
        tx.execute(
            "DELETE FROM pr_review_guide_versions WHERE id IN (
                SELECT v.id FROM pr_review_guide_versions v
                JOIN pr_review_guide_source_comparisons c ON c.id = v.comparison_id
                JOIN pr_review_guide_source_series s ON s.id = v.series_id
                LEFT JOIN tasks t ON t.id = s.root_task_id
                WHERE (t.id IS NULL OR t.deleted_at IS NOT NULL OR t.status IN ('done', 'archived'))
                  AND CAST(c.captured_at AS INTEGER) < ?1
                  AND NOT EXISTS (SELECT 1 FROM work_comments w WHERE w.guide_version_id = v.id)
            )",
            [cutoff],
        )?;
        tx.execute(
            "DELETE FROM pr_review_guide_attempts WHERE id IN (
                SELECT a.id FROM pr_review_guide_attempts a
                JOIN pr_review_guide_source_comparisons c ON c.id = a.comparison_id
                JOIN pr_review_guide_source_series s ON s.id = a.series_id
                LEFT JOIN tasks t ON t.id = s.root_task_id
                WHERE (t.id IS NULL OR t.deleted_at IS NOT NULL OR t.status IN ('done', 'archived'))
                  AND CAST(c.captured_at AS INTEGER) < ?1
                  AND NOT EXISTS (SELECT 1 FROM pr_review_guide_versions v WHERE v.attempt_id = a.id)
            )",
            [cutoff],
        )?;
        // Expire old evidence only for closed/deleted roots. Active series keep
        // their selection plus the most recent comparisons, regardless of age.
        tx.execute(
            "DELETE FROM pr_review_guide_source_comparisons WHERE id IN (
                SELECT c.id FROM pr_review_guide_source_comparisons c
                JOIN pr_review_guide_source_series s ON s.id = c.series_id
                LEFT JOIN tasks t ON t.id = s.root_task_id
                WHERE (t.id IS NULL OR t.deleted_at IS NOT NULL OR t.status IN ('done', 'archived'))
                  AND CAST(c.captured_at AS INTEGER) < ?1
                  AND NOT EXISTS (SELECT 1 FROM pr_review_guide_versions v WHERE v.comparison_id = c.id)
            )",
            [cutoff],
        )?;
        tx.execute(
            "DELETE FROM pr_review_guide_source_comparisons WHERE id IN (
                SELECT id FROM (
                    SELECT c.id, s.selected_comparison_id,
                           ROW_NUMBER() OVER (PARTITION BY c.series_id ORDER BY c.observation_sequence DESC, c.id DESC) AS rank
                    FROM pr_review_guide_source_comparisons c
                    JOIN pr_review_guide_source_series s ON s.id = c.series_id
                    WHERE c.id IS NOT s.selected_comparison_id
                      AND NOT EXISTS (SELECT 1 FROM pr_review_guide_versions v WHERE v.comparison_id = c.id)
                ) WHERE rank > ?1
            )", [policy.recent_comparisons],
        )?;
        tx.execute(
            "UPDATE pr_review_guide_source_series SET selected_comparison_id = NULL
            WHERE selected_comparison_id IS NOT NULL AND selected_comparison_id NOT IN
            (SELECT id FROM pr_review_guide_source_comparisons)",
            [],
        )?;
        tx.execute(
            "DELETE FROM pr_review_guide_source_series WHERE NOT EXISTS
            (SELECT 1 FROM pr_review_guide_source_comparisons c WHERE c.series_id = pr_review_guide_source_series.id)
            AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.id = root_task_id AND t.deleted_at IS NULL
                AND t.status NOT IN ('done', 'archived'))",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }
}
