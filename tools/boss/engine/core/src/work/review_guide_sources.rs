//! Durable source-packet capture for automatic PR review guides.
//!
//! Core owns series identity and observation ordering. The lower
//! `boss_pr_review_sources` crate owns packet construction and reference
//! validation, so database reconciliation never grows a second GitHub client.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

use super::query_ensure::RequireRow;
use super::*;
use boss_pr_review_sources::SourcePacket;

const PACKET_ARTIFACT_DIR: &str = "review-guide-sources";
const MAX_CAPTURE_ATTEMPTS: i64 = 3;
#[path = "review_guide_source_retention.rs"]
mod retention;
use retention::{SourceRetentionPolicy, packet_store_lock};

/// The lifecycle seam that requested a source capture. Kept with the durable
/// comparison row so diagnostics can tell an initial create observation from a
/// completion recovery or a later poller refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrSourceCaptureTrigger {
    Creation,
    Completion,
    Poller,
}

impl PrSourceCaptureTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Creation => "creation",
            Self::Completion => "completion",
            Self::Poller => "poller",
        }
    }
}

/// A persisted immutable packet plus the durable ordering and provenance that
/// selected it. This is the diagnostic read surface; it is intentionally not
/// a board projection and retains the full packet for source inspection.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PrReviewGuideSourceCapture {
    pub comparison_id: String,
    pub series_id: String,
    pub root_task_id: String,
    pub observation_sequence: i64,
    pub trigger: String,
    pub packet_hash: String,
    /// Settled packet: every requested side was read or omitted for a
    /// reason that is a property of the immutable revision. `true` does
    /// not mean every side has `content`.
    pub complete: bool,
    pub omission_count: i64,
    #[builder(default = 1)]
    pub attempt_count: i64,
    pub captured_at: String,
    pub packet_path: Option<String>,
    pub omission_summary: Option<String>,
    pub packet: SourcePacket,
}

/// Result of an idempotent packet persistence attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrSourceCapturePersistOutcome {
    Stored(PrReviewGuideSourceCapture),
    Existing(PrReviewGuideSourceCapture),
    IgnoredStaleObservation,
}

/// Additive persistence for the source-capture foundation. A series identifies
/// a canonical PR and its root task. A force-push, base advance, or later poll
/// preserves earlier comparisons; incomplete packets may be upgraded in place.
/// Rows are removed only when their referenced artifacts are unreadable.
pub(crate) fn migrate_pr_review_guide_source_capture_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pr_review_guide_source_series (
            id TEXT PRIMARY KEY,
            root_task_id TEXT NOT NULL,
            canonical_pr_url TEXT NOT NULL UNIQUE,
            latest_observation_sequence INTEGER NOT NULL DEFAULT 0,
            selected_comparison_id TEXT,
            last_capture_error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        DROP INDEX IF EXISTS pr_review_guide_source_series_root_idx;
        CREATE INDEX IF NOT EXISTS pr_review_guide_source_series_observation_idx
            ON pr_review_guide_source_series(root_task_id, latest_observation_sequence DESC, id DESC);
        CREATE TABLE IF NOT EXISTS pr_review_guide_source_comparisons (
            id TEXT PRIMARY KEY,
            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),
            observation_sequence INTEGER NOT NULL,
            observed_base_sha TEXT NOT NULL,
            merge_base_sha TEXT NOT NULL,
            head_sha TEXT NOT NULL,
            trigger TEXT NOT NULL,
            packet_hash TEXT NOT NULL,
            complete INTEGER NOT NULL CHECK (complete IN (0, 1)),
            omission_count INTEGER NOT NULL DEFAULT 0,
            packet_path TEXT,
            omission_summary_json TEXT,
            captured_at TEXT NOT NULL,
            UNIQUE(series_id, observed_base_sha, head_sha)
        );
        CREATE INDEX IF NOT EXISTS pr_review_guide_source_comparisons_series_sequence_idx
            ON pr_review_guide_source_comparisons(series_id, observation_sequence DESC, captured_at DESC);
        CREATE TABLE IF NOT EXISTS pr_review_guide_source_observation_sequence (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            last_sequence INTEGER NOT NULL
        );
        INSERT OR IGNORE INTO pr_review_guide_source_observation_sequence (id, last_sequence)
            VALUES (1, 0);",
    )?;
    // Databases created by the original capture PR used CREATE TABLE without
    // `omission_summary_json` and with a write-only `packet_json` column.
    // Fresh databases take the shape above; these two statements converge
    // already-created tables without re-adding columns that CREATE already
    // listed.
    if !table_has_column(conn, "pr_review_guide_source_comparisons", "omission_summary_json")? {
        conn.execute(
            "ALTER TABLE pr_review_guide_source_comparisons ADD COLUMN omission_summary_json TEXT",
            [],
        )?;
    }
    for (column, definition) in [
        ("probe_base_sha", "TEXT"),
        ("attempt_count", "INTEGER NOT NULL DEFAULT 1"),
    ] {
        if !table_has_column(conn, "pr_review_guide_source_comparisons", column)? {
            conn.execute(
                &format!("ALTER TABLE pr_review_guide_source_comparisons ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }
    if table_has_column(conn, "pr_review_guide_source_comparisons", "packet_json")? {
        conn.execute_batch(
            "UPDATE pr_review_guide_source_series SET selected_comparison_id = NULL
             WHERE selected_comparison_id IN
               (SELECT id FROM pr_review_guide_source_comparisons WHERE packet_path IS NULL);
             DELETE FROM pr_review_guide_source_comparisons WHERE packet_path IS NULL;
             ALTER TABLE pr_review_guide_source_comparisons DROP COLUMN packet_json;",
        )?;
    }
    Ok(())
}

impl WorkDb {
    /// Allocate the global observation order before starting an asynchronous
    /// source read. The returned sequence, rather than a response timestamp,
    /// fences delayed GitHub responses from replacing a newer observation.
    pub(crate) fn allocate_pr_review_guide_source_observation_sequence(&self) -> Result<i64> {
        let conn = self.connect()?;
        // The additive migration normally seeds this row. Repeating the
        // idempotent seed here also makes mixed-version test fixtures and an
        // interrupted first-open migration converge before allocating an
        // observation, rather than turning a missing seed into a silent
        // ordering gap.
        conn.execute(
            "INSERT OR IGNORE INTO pr_review_guide_source_observation_sequence (id, last_sequence)
             VALUES (1, 0)",
            [],
        )?;
        conn.query_row(
            "UPDATE pr_review_guide_source_observation_sequence
             SET last_sequence = last_sequence + 1
             WHERE id = 1
             RETURNING last_sequence",
            [],
            |row| row.get(0),
        )
        .context("allocate PR review-guide source observation sequence")
    }

    /// Resolve an execution to the canonical task that owns its PR series.
    /// Revision rows deliberately collapse to their first non-revision parent;
    /// the revision task itself must never create a second comparison series.
    pub(crate) fn review_guide_source_root_for_execution(&self, execution_id: &str) -> Result<String> {
        let conn = self.connect()?;
        let execution = query_execution(&conn, execution_id).require("execution", execution_id)?;
        let task = query_task(&conn, &execution.work_item_id).require("task", &execution.work_item_id)?;
        if task.kind == TaskKind::Revision {
            return chain_root(&conn, &task.id);
        }
        Ok(task.id)
    }

    /// Persist a packet only when its observation is at least as new as the
    /// series's latest observation. A delayed older poll receives an explicit
    /// no-op outcome; it cannot roll the desired comparison backwards.
    pub fn persist_pr_review_guide_source_capture(
        &self,
        root_task_id: &str,
        observation_sequence: i64,
        trigger: PrSourceCaptureTrigger,
        packet: &SourcePacket,
    ) -> Result<PrSourceCapturePersistOutcome> {
        self.persist_source_capture_with_publisher(
            root_task_id,
            observation_sequence,
            trigger,
            packet,
            publish_packet_artifact,
        )
    }

    fn persist_source_capture_with_publisher(
        &self,
        root_task_id: &str,
        observation_sequence: i64,
        trigger: PrSourceCaptureTrigger,
        packet: &SourcePacket,
        publish: impl FnOnce(&Path, &str, &[u8]) -> Result<String>,
    ) -> Result<PrSourceCapturePersistOutcome> {
        let mut publish = Some(publish);
        let conn = self.connect()?;
        let latest: Option<i64> = conn
            .query_row(
                "SELECT latest_observation_sequence FROM pr_review_guide_source_series
             WHERE canonical_pr_url = ?1 AND root_task_id = ?2",
                params![packet.canonical_pr_url, root_task_id],
                |row| row.get(0),
            )
            .optional()?;
        if latest.is_some_and(|latest| observation_sequence < latest) {
            return Ok(PrSourceCapturePersistOutcome::IgnoredStaleObservation);
        }
        drop(conn);
        let packet_bytes = serde_json::to_vec(packet).context("serialize captured PR source packet")?;
        let packet_hash = packet.content_hash()?;
        let complete = packet.is_complete();
        let omission_count = packet.omissions.len() as i64;
        let omission_summary =
            serde_json::to_string(&packet.omissions).context("serialize captured PR source omission summary")?;
        let artifact_root = self.artifact_root()?;
        let now = now_string();
        // Protect published-but-not-yet-referenced blobs across processes.
        // This store lock never serializes database-only operations.
        let _publication = packet_store_lock(&artifact_root, false)?;
        let mut published = None;
        loop {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing_series: Option<(String, String, i64)> = tx
                .query_row(
                    "SELECT id, root_task_id, latest_observation_sequence
                 FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1",
                    [&packet.canonical_pr_url],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let (series_id, latest_sequence) = match existing_series {
                Some((series_id, existing_root, latest_sequence)) => {
                    if existing_root != root_task_id {
                        bail!(
                            "canonical PR `{}` is already associated with root task `{existing_root}`, not `{root_task_id}`",
                            packet.canonical_pr_url,
                        );
                    }
                    (series_id, latest_sequence)
                }
                None => {
                    let series_id = next_id("prgs");
                    tx.execute(
                        "INSERT OR IGNORE INTO pr_review_guide_source_series
                     (id, root_task_id, canonical_pr_url, latest_observation_sequence, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 0, ?4, ?4)",
                        params![series_id, root_task_id, packet.canonical_pr_url, now],
                    )?;
                    let (series_id, existing_root, latest_sequence): (String, String, i64) = tx.query_row(
                        "SELECT id, root_task_id, latest_observation_sequence
                     FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1",
                        [&packet.canonical_pr_url],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?;
                    if existing_root != root_task_id {
                        bail!(
                            "canonical PR `{}` is already associated with root task `{existing_root}`, not `{root_task_id}`",
                            packet.canonical_pr_url,
                        );
                    }
                    (series_id, latest_sequence)
                }
            };
            if observation_sequence < latest_sequence {
                tx.commit()?;
                return Ok(PrSourceCapturePersistOutcome::IgnoredStaleObservation);
            }

            let existing = read_capture_by_endpoints(
                &tx,
                &artifact_root,
                &series_id,
                &packet.observed_base_sha,
                &packet.head_sha,
            )?;
            let needs_blob = existing
                .as_ref()
                .is_none_or(|existing| !existing.complete && (complete || omission_count < existing.omission_count));
            if needs_blob && published.is_none() {
                tx.commit()?;
                drop(conn);
                published = Some(publish.take().expect("publish once")(
                    &artifact_root,
                    &packet_hash,
                    &packet_bytes,
                )?);
                // Recheck both sequence and endpoint guards after publication.
                continue;
            }
            if let Some(mut existing) = existing {
                tx.execute(
                "UPDATE pr_review_guide_source_comparisons SET attempt_count = attempt_count + 1, probe_base_sha = ?2 WHERE id = ?1",
                params![existing.comparison_id, packet.probe_base_sha],
            )?;
                existing.attempt_count += 1;
                let should_upgrade = !existing.complete && (complete || omission_count < existing.omission_count);
                if should_upgrade {
                    let packet_path = published.take().expect("packet published outside transaction");
                    tx.execute(
                    "UPDATE pr_review_guide_source_comparisons
                     SET packet_hash = ?2, complete = ?3, omission_count = ?4, packet_path = ?5, omission_summary_json = ?6
                     WHERE id = ?1",
                    params![
                        existing.comparison_id,
                        packet_hash,
                        if complete { 1 } else { 0 },
                        omission_count,
                        packet_path.clone(),
                        omission_summary.clone(),
                    ],
                )?;
                    select_comparison(&tx, &series_id, &existing.comparison_id, observation_sequence, &now)?;
                    tx.commit()?;
                    let mut upgraded = existing;
                    upgraded.packet_hash = packet_hash;
                    upgraded.complete = complete;
                    upgraded.omission_count = omission_count;
                    upgraded.packet_path = Some(packet_path);
                    upgraded.omission_summary = Some(omission_summary);
                    upgraded.packet = packet.clone();
                    return Ok(PrSourceCapturePersistOutcome::Stored(upgraded));
                }
                select_comparison(&tx, &series_id, &existing.comparison_id, observation_sequence, &now)?;
                tx.commit()?;
                return Ok(PrSourceCapturePersistOutcome::Existing(existing));
            }

            let packet_path = published.take().expect("packet published outside transaction");
            let comparison_id = next_id("prgc");
            let capture = PrReviewGuideSourceCapture {
                comparison_id: comparison_id.clone(),
                series_id: series_id.clone(),
                root_task_id: root_task_id.to_owned(),
                observation_sequence,
                trigger: trigger.as_str().to_owned(),
                packet_hash: packet_hash.clone(),
                complete,
                omission_count,
                attempt_count: 1,
                captured_at: now.clone(),
                packet_path: Some(packet_path.clone()),
                omission_summary: Some(omission_summary.clone()),
                packet: packet.clone(),
            };
            tx.execute(
            "INSERT INTO pr_review_guide_source_comparisons
             (id, series_id, observation_sequence, observed_base_sha, merge_base_sha, head_sha,
              trigger, packet_hash, complete, omission_count, packet_path, omission_summary_json, captured_at, probe_base_sha)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                comparison_id,
                series_id,
                observation_sequence,
                packet.observed_base_sha,
                packet.merge_base_sha,
                packet.head_sha,
                trigger.as_str(),
                packet_hash,
                if complete { 1 } else { 0 },
                omission_count,
                packet_path,
                omission_summary,
                now,
                packet.probe_base_sha,
            ],
        )?;
            select_comparison(
                &tx,
                &capture.series_id,
                &capture.comparison_id,
                observation_sequence,
                &capture.captured_at,
            )?;
            tx.commit()?;
            return Ok(PrSourceCapturePersistOutcome::Stored(capture));
        }
    }

    /// Record a collection error even when metadata could not yield a packet.
    /// This is monotonic like successful persistence and supplies the precise
    /// omission/failure diagnostic rather than silently dropping a wake-up.
    pub fn record_pr_review_guide_source_capture_failure(
        &self,
        root_task_id: &str,
        canonical_pr_url: &str,
        observation_sequence: i64,
        error: &str,
    ) -> Result<bool> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT id, root_task_id, latest_observation_sequence
                 FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1",
                [canonical_pr_url],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (series_id, latest_sequence) = match existing {
            Some((series_id, existing_root, latest_sequence)) => {
                if existing_root != root_task_id {
                    bail!(
                        "canonical PR `{canonical_pr_url}` is already associated with root task `{existing_root}`, not `{root_task_id}`"
                    );
                }
                (series_id, latest_sequence)
            }
            None => {
                let series_id = next_id("prgs");
                tx.execute(
                    "INSERT OR IGNORE INTO pr_review_guide_source_series
                     (id, root_task_id, canonical_pr_url, latest_observation_sequence, last_capture_error, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 0, NULL, ?4, ?4)",
                    params![series_id, root_task_id, canonical_pr_url, now],
                )?;
                let (series_id, existing_root, latest_sequence): (String, String, i64) = tx.query_row(
                    "SELECT id, root_task_id, latest_observation_sequence
                     FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1",
                    [canonical_pr_url],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                if existing_root != root_task_id {
                    bail!(
                        "canonical PR `{canonical_pr_url}` is already associated with root task `{existing_root}`, not `{root_task_id}`"
                    );
                }
                (series_id, latest_sequence)
            }
        };
        if observation_sequence < latest_sequence {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE pr_review_guide_source_series
             SET latest_observation_sequence = ?2, last_capture_error = ?3, updated_at = ?4
             WHERE id = ?1",
            params![series_id, observation_sequence, error, now],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Diagnostic read path for an immutable packet. Returns the currently
    /// selected comparison for the requested canonical root; packet content
    /// remains in engine storage and is never re-fetched from a moving ref.
    pub fn get_latest_pr_review_guide_source_capture(
        &self,
        root_task_id: &str,
    ) -> Result<Option<PrReviewGuideSourceCapture>> {
        let conn = self.connect()?;
        let artifact_root = self.artifact_root()?;
        conn.query_row(
            "SELECT s.id, s.root_task_id, c.observation_sequence, c.trigger, c.packet_hash,
                    c.complete, c.captured_at, c.id, c.packet_path, c.omission_count, c.omission_summary_json, c.attempt_count
             FROM pr_review_guide_source_series s
             JOIN pr_review_guide_source_comparisons c ON c.id = s.selected_comparison_id
             WHERE s.root_task_id = ?1 ORDER BY s.latest_observation_sequence DESC, s.id DESC LIMIT 1",
            [root_task_id],
            |row| map_capture(row, &artifact_root),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Select a settled or retry-exhausted REST comparison.
    pub(crate) fn select_complete_pr_review_guide_source_capture(
        &self,
        root_task_id: &str,
        pr_url: &str,
        base_sha: &str,
        head_sha: &str,
        sequence: i64,
    ) -> Result<bool> {
        self.select_pr_review_guide_source_capture(root_task_id, pr_url, base_sha, head_sha, sequence, false)
    }

    /// Probe lookup avoids a metadata subprocess for unchanged observations.
    pub(crate) fn select_probe_pr_review_guide_source_capture(
        &self,
        root_task_id: &str,
        pr_url: &str,
        base_sha: &str,
        head_sha: &str,
        sequence: i64,
    ) -> Result<bool> {
        self.select_pr_review_guide_source_capture(root_task_id, pr_url, base_sha, head_sha, sequence, true)
    }

    fn select_pr_review_guide_source_capture(
        &self,
        root_task_id: &str,
        pr_url: &str,
        base_sha: &str,
        head_sha: &str,
        sequence: i64,
        probe: bool,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let existing: Option<(String, String, String, i64, String, Option<String>)> = conn
            .query_row(
                "SELECT s.id, c.id, s.root_task_id, s.latest_observation_sequence, c.packet_hash, c.packet_path
             FROM pr_review_guide_source_series s
             JOIN pr_review_guide_source_comparisons c ON c.series_id = s.id
             WHERE s.canonical_pr_url = ?1 AND c.head_sha = ?3
             AND (CASE WHEN ?4 THEN COALESCE(c.probe_base_sha, c.observed_base_sha) ELSE c.observed_base_sha END) = ?2
             AND (c.complete = 1 OR c.attempt_count >= ?5)
             ORDER BY c.observation_sequence DESC LIMIT 1",
                params![pr_url, base_sha, head_sha, probe, MAX_CAPTURE_ATTEMPTS],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((series, comparison, root, _, hash, path)) = existing else {
            return Ok(false);
        };
        anyhow::ensure!(
            root == root_task_id,
            "canonical PR is already associated with another root task"
        );
        drop(conn);
        let validation = load_packet(&self.artifact_root()?, path.as_deref(), &hash);
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let latest: Option<i64> = tx
            .query_row(
                "SELECT s.latest_observation_sequence FROM pr_review_guide_source_series s
             JOIN pr_review_guide_source_comparisons c ON c.series_id = s.id
             WHERE s.id = ?1 AND c.id = ?2 AND c.packet_hash = ?3 AND c.packet_path IS ?4 AND (c.complete = 1 OR c.attempt_count >= ?5)",
                params![series, comparison, hash, path, MAX_CAPTURE_ATTEMPTS],
                |row| row.get(0),
            )
            .optional()?;
        let Some(latest) = latest else {
            return Ok(false);
        };
        if let Err(error) = validation {
            if let Some(path) = &path {
                match fs::remove_file(self.artifact_root()?.join(path)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            // Forget the unusable comparison so the next collection can replace it.
            tx.execute(
                "DELETE FROM pr_review_guide_source_comparisons WHERE id = ?1",
                [&comparison],
            )?;
            tx.execute(
                "UPDATE pr_review_guide_source_series
                 SET selected_comparison_id = CASE WHEN selected_comparison_id = ?2 THEN NULL ELSE selected_comparison_id END,
                     last_capture_error = CASE WHEN selected_comparison_id = ?2 OR latest_observation_sequence <= ?4
                         THEN ?3 ELSE last_capture_error END
                 WHERE id = ?1",
                params![series, comparison, error.to_string(), sequence],
            )?;
            tx.commit()?;
            tracing::warn!(%error, pr_url, "invalid source artifact; recollecting comparison");
            return Ok(false);
        }
        if sequence >= latest {
            select_comparison(&tx, &series, &comparison, sequence, &now_string())?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// A collector error still consumes a retry of existing incomplete evidence.
    pub(crate) fn record_pr_review_guide_source_retry_error(
        &self,
        pr_url: &str,
        endpoints: &boss_pr_review_sources::PinnedComparison,
    ) -> Result<()> {
        self.connect()?.execute(
            "UPDATE pr_review_guide_source_comparisons SET attempt_count = attempt_count + 1
             WHERE observed_base_sha = ?2 AND head_sha = ?3 AND complete = 0
             AND series_id IN (SELECT id FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1)",
            params![pr_url, endpoints.base_sha, endpoints.head_sha],
        )?;
        Ok(())
    }

    pub(crate) fn remember_pr_review_guide_probe(
        &self,
        pr_url: &str,
        endpoints: &boss_pr_review_sources::PinnedComparison,
        probe_base: &str,
        sequence: i64,
    ) -> Result<()> {
        self.connect()?.execute(
            "UPDATE pr_review_guide_source_comparisons SET probe_base_sha = ?4
             WHERE observed_base_sha = ?2 AND head_sha = ?3 AND series_id IN
             (SELECT id FROM pr_review_guide_source_series WHERE canonical_pr_url = ?1 AND latest_observation_sequence = ?5)",
            params![pr_url, endpoints.base_sha, endpoints.head_sha, probe_base, sequence],
        )?;
        Ok(())
    }

    fn artifact_root(&self) -> Result<PathBuf> {
        self.path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .with_context(|| {
                format!(
                    "work db `{}` has no parent directory for source-packet artifacts",
                    self.path.display()
                )
            })
    }

    /// Belt-and-braces sweep for packet blobs orphaned by a crash between
    /// publish and commit. Collects the live path set under `connect()`,
    /// then walks the store after dropping that guard so the process-wide
    /// connection mutex is not held for a directory walk.
    pub fn gc_unreferenced_pr_review_guide_source_artifacts(&self) -> Result<()> {
        let artifact_root = self.artifact_root()?;
        // connect() is process-local. The store lock also excludes publishers
        // in other processes; skip this pass if a publisher is active.
        let Some(_gc) = packet_store_lock(&artifact_root, true)? else {
            return Ok(());
        };
        self.prune_pr_review_guide_sources(SourceRetentionPolicy::default())?;
        delete_orphan_tmp_packet_artifacts(&artifact_root)?;
        let live = {
            let conn = self.connect()?;
            live_packet_paths(&conn)?
        };
        let candidates = unreferenced_packet_blob_paths(&artifact_root, &live);
        let conn = self.connect()?;
        for relative in candidates {
            delete_unreferenced_packet_artifact(&conn, &artifact_root, &relative)?;
        }
        Ok(())
    }
}

fn select_comparison(
    tx: &rusqlite::Transaction<'_>,
    series_id: &str,
    comparison_id: &str,
    observation_sequence: i64,
    now: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE pr_review_guide_source_series
         SET latest_observation_sequence = MAX(latest_observation_sequence, ?2),
             last_capture_error = NULL,
             selected_comparison_id = ?3,
             updated_at = ?4
         WHERE id = ?1",
        params![series_id, observation_sequence, comparison_id, now],
    )?;
    Ok(())
}

fn read_capture_by_endpoints(
    conn: &Connection,
    artifact_root: &Path,
    series_id: &str,
    base_sha: &str,
    head_sha: &str,
) -> Result<Option<PrReviewGuideSourceCapture>> {
    let metadata = conn
        .query_row(
            "SELECT s.id, s.root_task_id, c.observation_sequence, c.trigger, c.packet_hash,
                c.complete, c.captured_at, c.id, c.packet_path, c.omission_count, c.omission_summary_json, c.attempt_count
         FROM pr_review_guide_source_series s
         JOIN pr_review_guide_source_comparisons c ON c.series_id = s.id
         WHERE c.series_id = ?1 AND c.observed_base_sha = ?2 AND c.head_sha = ?3",
            params![series_id, base_sha, head_sha],
            CaptureMetadata::from_row,
        )
        .optional()?;
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    match load_packet(artifact_root, metadata.packet_path.as_deref(), &metadata.packet_hash) {
        Ok(packet) => Ok(Some(metadata.with_packet(packet))),
        Err(error) => {
            if let Some(path) = &metadata.packet_path {
                match fs::remove_file(artifact_root.join(path)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            conn.execute("UPDATE pr_review_guide_source_series SET selected_comparison_id = NULL WHERE id = ?1 AND selected_comparison_id = ?2", params![series_id, metadata.comparison_id])?;
            conn.execute(
                "DELETE FROM pr_review_guide_source_comparisons WHERE id = ?1",
                [&metadata.comparison_id],
            )?;
            tracing::warn!(%error, comparison_id = metadata.comparison_id, "invalid source artifact; replacing comparison");
            Ok(None)
        }
    }
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct CaptureMetadata {
    series_id: String,
    root_task_id: String,
    observation_sequence: i64,
    trigger: String,
    packet_hash: String,
    complete: bool,
    captured_at: String,
    comparison_id: String,
    packet_path: Option<String>,
    omission_count: i64,
    attempt_count: i64,
    omission_summary: Option<String>,
}

impl CaptureMetadata {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            series_id: row.get(0)?,
            root_task_id: row.get(1)?,
            observation_sequence: row.get(2)?,
            trigger: row.get(3)?,
            packet_hash: row.get(4)?,
            complete: row.get::<_, i64>(5)? != 0,
            captured_at: row.get(6)?,
            comparison_id: row.get(7)?,
            packet_path: row.get(8)?,
            omission_count: row.get(9)?,
            attempt_count: row.get(11)?,
            omission_summary: row.get(10)?,
        })
    }

    fn with_packet(self, packet: SourcePacket) -> PrReviewGuideSourceCapture {
        PrReviewGuideSourceCapture {
            series_id: self.series_id,
            root_task_id: self.root_task_id,
            observation_sequence: self.observation_sequence,
            trigger: self.trigger,
            packet_hash: self.packet_hash,
            complete: self.complete,
            captured_at: self.captured_at,
            comparison_id: self.comparison_id,
            packet_path: self.packet_path,
            omission_count: self.omission_count,
            attempt_count: self.attempt_count,
            omission_summary: self.omission_summary,
            packet,
        }
    }
}

fn map_capture(row: &Row<'_>, artifact_root: &Path) -> rusqlite::Result<PrReviewGuideSourceCapture> {
    let metadata = CaptureMetadata::from_row(row)?;
    let packet =
        load_packet(artifact_root, metadata.packet_path.as_deref(), &metadata.packet_hash).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(error.to_string())),
            )
        })?;
    Ok(metadata.with_packet(packet))
}

#[cfg(test)]
thread_local! {
    static BEFORE_PACKET_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

fn load_packet(artifact_root: &Path, packet_path: Option<&str>, expected_hash: &str) -> Result<SourcePacket> {
    #[cfg(test)]
    BEFORE_PACKET_READ.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let packet_path = packet_path
        .filter(|path| !path.is_empty())
        .context("missing referenced source packet blob: no artifact path")?;
    let path = artifact_root.join(packet_path);
    let bytes =
        fs::read(&path).with_context(|| format!("missing referenced source packet blob at {}", path.display()))?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == expected_hash,
        "source packet integrity failure at {}: digest mismatch",
        path.display()
    );
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse source packet blob at {}", path.display()))
}

fn publish_packet_artifact(state_root: &Path, packet_hash: &str, bytes: &[u8]) -> Result<String> {
    anyhow::ensure!(packet_hash.len() >= 2, "source packet hash is too short to address");
    let relative = format!("{PACKET_ARTIFACT_DIR}/{}/{}", &packet_hash[..2], packet_hash);
    let dest = state_root.join(&relative);
    if !dest.exists() || load_packet(state_root, Some(&relative), packet_hash).is_err() {
        boss_engine_utils::atomic_blob::write_blob_atomic(&dest, bytes)?;
    }
    Ok(relative)
}

fn live_packet_paths(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT packet_path FROM pr_review_guide_source_comparisons
         WHERE packet_path IS NOT NULL AND packet_path != ''",
    )?;
    Ok(stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn delete_unreferenced_packet_artifact(conn: &Connection, artifact_root: &Path, relative: &str) -> Result<()> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pr_review_guide_source_comparisons WHERE packet_path = ?1",
        [relative],
        |row| row.get(0),
    )?;
    if count == 0 {
        let _ = fs::remove_file(artifact_root.join(relative));
        if let Some(shard) = artifact_root.join(relative).parent()
            && fs::read_dir(shard)
                .ok()
                .is_some_and(|mut entries| entries.next().is_none())
        {
            let _ = fs::remove_dir(shard);
        }
    }
    Ok(())
}

fn delete_orphan_tmp_packet_artifacts(artifact_root: &Path) -> Result<()> {
    let root = artifact_root.join(PACKET_ARTIFACT_DIR);
    let Ok(shards) = fs::read_dir(&root) else {
        return Ok(());
    };
    for shard in shards.flatten() {
        let shard_path = shard.path();
        if !shard_path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&shard_path) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let name = name.to_string_lossy();
            let pid = name
                .strip_suffix(".tmp")
                .and_then(|stem| stem.rsplit_once('.'))
                .and_then(|(stem, _)| stem.rsplit_once('.'))
                .and_then(|(_, pid)| pid.parse::<i32>().ok());
            if let Some(pid) = pid
                && pid > 0
                && pid != std::process::id() as i32
                && matches!(
                    crate::dead_pid_sweep::probe_pid(pid),
                    crate::dead_pid_sweep::PidStatus::Dead
                )
            {
                let _ = fs::remove_file(file.path());
            }
        }
    }
    Ok(())
}

fn unreferenced_packet_blob_paths(artifact_root: &Path, live: &std::collections::HashSet<String>) -> Vec<String> {
    let root = artifact_root.join(PACKET_ARTIFACT_DIR);
    let Ok(shards) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for shard in shards.flatten() {
        let shard_path = shard.path();
        if !shard_path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&shard_path) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let name = file.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".tmp") {
                // Staging files require the separate PID-aware sweep.
                continue;
            }
            if let Some(relative) = path.strip_prefix(artifact_root).ok().and_then(|p| p.to_str())
                && !live.contains(relative)
            {
                candidates.push(relative.to_owned());
            }
        }
    }
    candidates
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod artifact_tests;

#[cfg(test)]
#[path = "review_guide_sources_tests.rs"]
mod upstream_tests;
