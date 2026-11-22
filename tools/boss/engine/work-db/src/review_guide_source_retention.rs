//! Retention and cross-process publication fencing for source packets.
use super::*;
use fs4::fs_std::FileExt;

pub(crate) const RETENTION_INDEXES_MIGRATION_ID: &str = "20261122_0515_review_guide_retention_indexes";

pub(crate) fn migrate_retention_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS pr_review_guide_versions_attempt_idx
             ON pr_review_guide_versions(attempt_id);
         CREATE INDEX IF NOT EXISTS pr_review_guide_versions_comparison_idx
             ON pr_review_guide_versions(comparison_id);
         CREATE INDEX IF NOT EXISTS pr_review_guide_updates_previous_version_idx
             ON pr_review_guide_updates(previous_version_id);",
    )?;
    Ok(())
}

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
/// Explicitly unlock before closing: a concurrent fork can inherit the file
/// description and keep a close-only lock alive until the child execs/exits.
pub(super) struct PacketStoreLock(fs::File);

impl Drop for PacketStoreLock {
    fn drop(&mut self) {
        if let Err(err) = FileExt::unlock(&self.0) {
            tracing::warn!(?err, "could not release source packet store lock");
        }
    }
}

pub(super) fn packet_store_lock(root: &Path, exclusive: bool) -> Result<Option<PacketStoreLock>> {
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
    Ok(Some(PacketStoreLock(file)))
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub(super) fn prune_pr_review_guide_sources(&self, policy: SourceRetentionPolicy) -> Result<()> {
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let cutoff = now - policy.terminal_age_seconds;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Terminal, aged series: drop published guide versions that no
        // `work_comments` row still cites, then their attempts, so the
        // comparison DELETE below can reclaim the packet blob. A manual run
        // on a completed PR may reuse old sources: retain fresh versions and
        // live/recent attempts independently of the source packet's age.
        // Versions that still have comments keep the comparison (RESTRICT) and therefore
        // the on-disk packet. Active series are untouched here.
        tx.execute(
            "DELETE FROM pr_review_guide_versions WHERE id IN (
                SELECT v.id FROM pr_review_guide_versions v
                JOIN pr_review_guide_source_comparisons c ON c.id = v.comparison_id
                JOIN pr_review_guide_source_series s ON s.id = v.series_id
                LEFT JOIN tasks t ON t.id = s.root_task_id
                WHERE (t.id IS NULL OR t.deleted_at IS NOT NULL OR t.status IN ('done', 'archived'))
                  AND CAST(c.captured_at AS INTEGER) < ?1
                  AND CAST(v.generated_at AS INTEGER) < ?1
                  AND NOT EXISTS (SELECT 1 FROM work_comments w WHERE w.guide_version_id = v.id)
                  AND NOT EXISTS (SELECT 1 FROM pr_review_guide_updates u
                      JOIN pr_review_guide_attempts a ON a.id = u.attempt_id
                      WHERE u.previous_version_id = v.id AND a.status IN ('queued', 'running'))
                  -- The readable version is never collected, whatever the lifecycle,
                  -- until its root is deleted.
                  AND (s.readable_version_id IS NOT v.id OR t.id IS NULL OR t.deleted_at IS NOT NULL)
            )",
            [cutoff],
        )?;
        tx.execute(
            "UPDATE pr_review_guide_source_series SET readable_version_id = NULL
             WHERE readable_version_id IS NOT NULL
               AND readable_version_id NOT IN (SELECT id FROM pr_review_guide_versions)",
            [],
        )?;
        tx.execute(
            "DELETE FROM pr_review_guide_attempts WHERE id IN (
                SELECT a.id FROM pr_review_guide_attempts a
                JOIN pr_review_guide_source_comparisons c ON c.id = a.comparison_id
                JOIN pr_review_guide_source_series s ON s.id = a.series_id
                LEFT JOIN tasks t ON t.id = s.root_task_id
                WHERE (t.id IS NULL OR t.deleted_at IS NOT NULL OR t.status IN ('done', 'archived'))
                  AND CAST(c.captured_at AS INTEGER) < ?1
                  AND a.status IN ('succeeded', 'failed', 'cancelled', 'superseded')
                  AND CAST(a.created_at AS INTEGER) < ?1
                  AND NOT EXISTS (SELECT 1 FROM pr_review_guide_versions v WHERE v.attempt_id = a.id)
                  -- The current request of a series with a readable guide carries
                  -- the update/failure status and is what Retry re-admits from.
                  AND NOT (s.readable_version_id IS NOT NULL
                      AND t.id IS NOT NULL AND t.deleted_at IS NULL
                      AND a.request_epoch = s.request_epoch
                      AND a.comparison_id IS s.selected_comparison_id)
            )",
            [cutoff],
        )?;
        // Published versions on an active series are not collected by age or
        // recency. An open viewer and an in-memory draft pin neither a
        // `work_comments` row nor `readable_version_id`, so a recency cap
        // would delete the version a draft still needs. Owning-history
        // deletion (terminal-aged series above) is the only collector for
        // published versions.
        tx.execute(
            "DELETE FROM pr_review_guide_attempts
             WHERE status IN ('succeeded', 'failed', 'cancelled', 'superseded')
               AND NOT EXISTS (SELECT 1 FROM pr_review_guide_versions v WHERE v.attempt_id = pr_review_guide_attempts.id)
               AND comparison_id IN (
                   SELECT id FROM (
                       SELECT c.id, ROW_NUMBER() OVER (
                           PARTITION BY c.series_id ORDER BY c.observation_sequence DESC, c.id DESC
                       ) AS rank
                       FROM pr_review_guide_source_comparisons c
                       JOIN pr_review_guide_source_series s ON s.id = c.series_id
                       WHERE c.id IS NOT s.selected_comparison_id
                   ) WHERE rank > ?1
               )",
            [policy.recent_comparisons],
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
                  AND NOT EXISTS (SELECT 1 FROM pr_review_guide_attempts a WHERE a.comparison_id = c.id)
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
                ) WHERE rank > ?1
            )
            AND NOT EXISTS (SELECT 1 FROM pr_review_guide_versions v WHERE v.comparison_id = pr_review_guide_source_comparisons.id)
            AND NOT EXISTS (SELECT 1 FROM pr_review_guide_attempts a WHERE a.comparison_id = pr_review_guide_source_comparisons.id)", [policy.recent_comparisons],
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

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn publisher_unlocks_even_when_an_inherited_descriptor_remains_open() {
        let root = tempfile::tempdir().unwrap();
        let publisher = packet_store_lock(root.path(), false).unwrap().unwrap();
        // dup and fork both retain the same open file description.
        let inherited = publisher.0.try_clone().unwrap();
        assert!(packet_store_lock(root.path(), true).unwrap().is_none());
        drop(publisher);
        let gc = packet_store_lock(root.path(), true)
            .unwrap()
            .expect("finished publisher must release its lock before inherited descriptors close");
        drop(inherited);
        drop(gc);
    }

    #[test]
    fn releasing_one_publisher_preserves_another_publishers_lock() {
        let root = tempfile::tempdir().unwrap();
        let first = packet_store_lock(root.path(), false).unwrap().unwrap();
        let second = packet_store_lock(root.path(), false).unwrap().unwrap();
        drop(first);
        assert!(packet_store_lock(root.path(), true).unwrap().is_none());
        drop(second);
        assert!(packet_store_lock(root.path(), true).unwrap().is_some());
    }

    #[test]
    fn prune_pr_review_guide_sources_avoids_quadratic_nested_loop_scans() {
        let (_dir, db) = crate::test_support::open_db();
        let now = boss_engine_utils::epoch_time::now_epoch_secs().to_string();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "INSERT INTO pr_review_guide_source_series
                 (id, root_task_id, canonical_pr_url, created_at, updated_at)
                 VALUES ('ser_1', 'task_1', 'https://github.com/o/r/pull/1', ?1, ?1)",
                [&now],
            )
            .unwrap();
            for i in 0..250 {
                let cmp_id = format!("cmp_{i}");
                let att_id = format!("att_{i}");
                let ver_id = format!("ver_{i}");
                let sha = format!("{i:040x}");
                conn.execute(
                    "INSERT INTO pr_review_guide_source_comparisons
                     (id, series_id, observation_sequence, observed_base_sha, merge_base_sha,
                      head_sha, trigger, packet_hash, complete, captured_at)
                     VALUES (?1, 'ser_1', ?2, 'base', 'base', ?3, 'manual', 'hash', 1, ?4)",
                    rusqlite::params![cmp_id, i, sha, now],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO pr_review_guide_attempts
                     (id, series_id, comparison_id, request_epoch, ordinal, status,
                      prompt_version, created_at)
                     VALUES (?1, 'ser_1', ?2, 1, 1, 'succeeded', 'v1', ?3)",
                    rusqlite::params![att_id, cmp_id, now],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO pr_review_guide_versions
                     (id, series_id, comparison_id, attempt_id, markdown, raw_output,
                      content_hash, prompt_version, generated_at)
                     VALUES (?1, 'ser_1', ?2, ?3, 'md', 'raw', 'ch', 'v1', ?4)",
                    rusqlite::params![ver_id, cmp_id, att_id, now],
                )
                .unwrap();
            }
        }
        let ((), measured) = crate::db_diagnostics::read_volume::measure(|| {
            db.prune_pr_review_guide_sources(SourceRetentionPolicy::default())
                .unwrap();
        });
        assert!(
            measured.full_scan_steps < 5_000,
            "prune_pr_review_guide_sources must not scan quadratically over versions/attempts/comparisons; got {} full_scan_steps (top={:?})",
            measured.full_scan_steps,
            measured.top_statement()
        );
    }
}
