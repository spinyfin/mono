//! Retention and cross-process publication fencing for source packets.
use std::time::Duration;

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
/// share it for the span of one capture transaction; GC needs it exclusively.
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

/// Retry attempts for [`packet_store_lock_for_gc`]. A publisher holds the
/// shared lock only for one capture transaction (a quick blob write plus a
/// couple of short SQLite transactions), so a handful of short backoffs is
/// enough slack to ride out real contention.
const GC_LOCK_RETRY_ATTEMPTS: u32 = 6;
const GC_LOCK_RETRY_BASE_DELAY: Duration = Duration::from_millis(25);

/// Exclusive store lock for the GC sweep, with bounded backoff instead of a
/// single non-blocking attempt.
///
/// GC deliberately does not block indefinitely on the exclusive lock: a
/// blocking acquire here would turn a stuck or crash-orphaned publisher (the
/// OS releases the lock on process exit, but a hung process still holds it)
/// into a hung GC sweep instead of a merely-late one. But the previous
/// behavior — returning success without collecting anything the instant the
/// lock was busy even once — meant a caller had no way to tell "GC ran" from
/// "GC silently declined to run", so *every* contended pass leaked its
/// garbage forever from the caller's point of view. Retrying with a bounded
/// backoff lets a normal, fast in-flight publish clear before we give up,
/// and returning an error (rather than `Ok(None)`) when contention outlasts
/// the retry budget makes the remaining "still busy" case loud instead of
/// silent: the caller must treat it as a failed pass, not a completed one.
pub(super) fn packet_store_lock_for_gc(root: &Path) -> Result<fs::File> {
    let directory = root.join(PACKET_ARTIFACT_DIR);
    fs::create_dir_all(&directory)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("publication.lock"))?;
    for attempt in 0..GC_LOCK_RETRY_ATTEMPTS {
        if FileExt::try_lock_exclusive(&file)? {
            return Ok(file);
        }
        if attempt + 1 < GC_LOCK_RETRY_ATTEMPTS {
            std::thread::sleep(GC_LOCK_RETRY_BASE_DELAY * 2u32.pow(attempt));
        }
    }
    bail!(
        "review-guide source GC could not acquire the exclusive packet store lock \
         after {GC_LOCK_RETRY_ATTEMPTS} attempts; a publisher held it the whole time"
    );
}

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
            "UPDATE pr_review_guide_source_series SET readable_version_id = (
                SELECT retained.id FROM pr_review_guide_versions retained
                WHERE retained.series_id = pr_review_guide_source_series.id
                  AND EXISTS (SELECT 1 FROM work_comments w WHERE w.guide_version_id = retained.id)
                ORDER BY CAST(retained.generated_at AS INTEGER) DESC, retained.rowid DESC LIMIT 1
             )
             WHERE readable_version_id IN (
                SELECT v.id FROM pr_review_guide_versions v
                JOIN pr_review_guide_source_comparisons c ON c.id = v.comparison_id
                JOIN pr_review_guide_source_series s ON s.id = v.series_id
                LEFT JOIN tasks t ON t.id = s.root_task_id
                WHERE (t.id IS NULL OR t.deleted_at IS NOT NULL OR t.status IN ('done', 'archived'))
                  AND CAST(c.captured_at AS INTEGER) < ?1
                  AND CAST(v.generated_at AS INTEGER) < ?1
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
                  AND CAST(v.generated_at AS INTEGER) < ?1
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
                  AND a.status IN ('succeeded', 'failed', 'cancelled', 'superseded')
                  AND CAST(a.created_at AS INTEGER) < ?1
                  AND NOT EXISTS (SELECT 1 FROM pr_review_guide_versions v WHERE v.attempt_id = a.id)
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
