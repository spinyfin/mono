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
    pub complete: bool,
    pub omission_count: i64,
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
/// a canonical PR and its root task; comparisons are separately immutable so
/// a force-push, base advance, or later poll cannot erase historical evidence.
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
        CREATE INDEX IF NOT EXISTS pr_review_guide_source_series_root_idx
            ON pr_review_guide_source_series(root_task_id, updated_at DESC);
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
    if table_has_column(conn, "pr_review_guide_source_comparisons", "packet_json")? {
        conn.execute(
            "ALTER TABLE pr_review_guide_source_comparisons DROP COLUMN packet_json",
            [],
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
        let packet_bytes = serde_json::to_vec(packet).context("serialize captured PR source packet")?;
        let packet_hash = packet.content_hash()?;
        let complete = packet.is_complete();
        let omission_count = packet.omissions.len() as i64;
        let omission_summary =
            serde_json::to_string(&packet.omissions).context("serialize captured PR source omission summary")?;
        let artifact_root = self.artifact_root()?;
        let now = now_string();
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
        if let Some(existing) = existing {
            let should_upgrade = !existing.complete && (complete || omission_count < existing.omission_count);
            if should_upgrade {
                let packet_path = publish_packet_artifact(&artifact_root, &packet_hash, &packet_bytes)?;
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
                gc_unreferenced_packet_artifacts(&conn, &artifact_root)?;
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

        let packet_path = publish_packet_artifact(&artifact_root, &packet_hash, &packet_bytes)?;
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
            captured_at: now.clone(),
            packet_path: Some(packet_path.clone()),
            omission_summary: Some(omission_summary.clone()),
            packet: packet.clone(),
        };
        tx.execute(
            "INSERT INTO pr_review_guide_source_comparisons
             (id, series_id, observation_sequence, observed_base_sha, merge_base_sha, head_sha,
              trigger, packet_hash, complete, omission_count, packet_path, omission_summary_json, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
        gc_unreferenced_packet_artifacts(&conn, &artifact_root)?;
        Ok(PrSourceCapturePersistOutcome::Stored(capture))
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
                    c.complete, c.captured_at, c.id, c.packet_path, c.omission_count, c.omission_summary_json
             FROM pr_review_guide_source_series s
             JOIN pr_review_guide_source_comparisons c ON c.id = s.selected_comparison_id
             WHERE s.root_task_id = ?1 ORDER BY s.updated_at DESC, s.id DESC LIMIT 1",
            [root_task_id],
            |row| map_capture(row, &artifact_root),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Select an already complete comparison and fence older in-flight observations.
    pub(crate) fn select_complete_pr_review_guide_source_capture(
        &self,
        root_task_id: &str,
        pr_url: &str,
        base_sha: &str,
        head_sha: &str,
        sequence: i64,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, String, i64)> = tx
            .query_row(
                "SELECT s.id, c.id, s.root_task_id, s.latest_observation_sequence
             FROM pr_review_guide_source_series s
             JOIN pr_review_guide_source_comparisons c ON c.series_id = s.id
             WHERE s.canonical_pr_url = ?1 AND c.observed_base_sha = ?2 AND c.head_sha = ?3 AND c.complete = 1",
                params![pr_url, base_sha, head_sha],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((series, comparison, root, latest)) = existing else {
            return Ok(false);
        };
        anyhow::ensure!(
            root == root_task_id,
            "canonical PR is already associated with another root task"
        );
        if sequence >= latest {
            select_comparison(&tx, &series, &comparison, sequence, &now_string())?;
        }
        tx.commit()?;
        Ok(true)
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
    conn.query_row(
        "SELECT s.id, s.root_task_id, c.observation_sequence, c.trigger, c.packet_hash,
                c.complete, c.captured_at, c.id, c.packet_path, c.omission_count, c.omission_summary_json
         FROM pr_review_guide_source_series s
         JOIN pr_review_guide_source_comparisons c ON c.series_id = s.id
         WHERE c.series_id = ?1 AND c.observed_base_sha = ?2 AND c.head_sha = ?3",
        params![series_id, base_sha, head_sha],
        |row| map_capture(row, artifact_root),
    )
    .optional()
    .map_err(Into::into)
}

fn map_capture(row: &Row<'_>, artifact_root: &Path) -> rusqlite::Result<PrReviewGuideSourceCapture> {
    let packet_hash: String = row.get(4)?;
    let packet_path: Option<String> = row.get(8)?;
    let packet = load_packet(artifact_root, packet_path.as_deref(), &packet_hash).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(error.to_string())),
        )
    })?;
    Ok(PrReviewGuideSourceCapture {
        series_id: row.get(0)?,
        root_task_id: row.get(1)?,
        observation_sequence: row.get(2)?,
        trigger: row.get(3)?,
        packet_hash,
        omission_count: row.get(9)?,
        complete: row.get::<_, i64>(5)? != 0,
        captured_at: row.get(6)?,
        comparison_id: row.get(7)?,
        packet_path,
        omission_summary: row.get(10)?,
        packet,
    })
}

fn load_packet(artifact_root: &Path, packet_path: Option<&str>, expected_hash: &str) -> Result<SourcePacket> {
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
    if dest.exists() {
        load_packet(state_root, Some(&relative), packet_hash)?;
    } else {
        boss_engine_utils::atomic_blob::write_blob_atomic(&dest, bytes)?;
    }
    Ok(relative)
}

fn gc_unreferenced_packet_artifacts(conn: &Connection, artifact_root: &Path) -> Result<()> {
    let live: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare(
            "SELECT packet_path FROM pr_review_guide_source_comparisons
             WHERE packet_path IS NOT NULL AND packet_path != ''",
        )?;
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let root = artifact_root.join(PACKET_ARTIFACT_DIR);
    let Ok(shards) = fs::read_dir(&root) else {
        return Ok(());
    };
    for shard in shards.flatten() {
        let shard_path = shard.path();
        if !shard_path.is_dir() {
            let _ = fs::remove_file(&shard_path);
            continue;
        }
        let Ok(files) = fs::read_dir(&shard_path) else {
            continue;
        };
        let mut empty = true;
        for file in files.flatten() {
            let path = file.path();
            let name = file.file_name();
            let name = name.to_string_lossy();
            let relative = path
                .strip_prefix(artifact_root)
                .ok()
                .and_then(|p| p.to_str())
                .map(str::to_owned);
            if name.ends_with(".tmp") || relative.as_ref().is_none_or(|rel| !live.contains(rel)) {
                let _ = fs::remove_file(&path);
            } else {
                empty = false;
            }
        }
        if empty {
            let _ = fs::remove_dir(&shard_path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_active_chore, create_product, open_db};
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_pr_review_sources::{ChangeKind, PinnedSource, SourceFile, SourceOmission, SourceSide};
    use boss_protocol::{CreateExecutionInput, CreateRevisionInput, ExecutionKind, ExecutionStatus, WorkItemPatch};

    fn packet(base: &str, head: &str) -> SourcePacket {
        SourcePacket {
            schema_version: 2,
            canonical_pr_url: "https://github.com/acme/widget/pull/11".to_owned(),
            pr_number: 11,
            title: "Capture immutable comparison".to_owned(),
            body: Some("body".to_owned()),
            base_repository: "acme/widget".to_owned(),
            head_repository: "acme/widget".to_owned(),
            observed_base_sha: base.to_owned(),
            merge_base_sha: "merge-base".to_owned(),
            head_sha: head.to_owned(),
            files: vec![SourceFile {
                path: "src/lib.rs".to_owned(),
                previous_path: None,
                change_kind: ChangeKind::Modified,
                additions: 1,
                deletions: 1,
                patch: Some("@@".to_owned()),
                before: Some(
                    PinnedSource::builder()
                        .repository("acme/widget")
                        .sha("merge-base")
                        .path("src/lib.rs")
                        .object_sha("before-object")
                        .content("before\n")
                        .content_hash("before-hash")
                        .byte_count(7)
                        .build(),
                ),
                after: Some(
                    PinnedSource::builder()
                        .repository("acme/widget")
                        .sha(head)
                        .path("src/lib.rs")
                        .object_sha("after-object")
                        .content("after\n")
                        .content_hash("after-hash")
                        .byte_count(6)
                        .build(),
                ),
            }],
            omissions: Vec::new(),
        }
    }

    #[test]
    fn captures_series_by_canonical_root_and_rejects_stale_observations() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "capture source packet");
        let stored = db
            .persist_pr_review_guide_source_capture(
                &root,
                7,
                PrSourceCaptureTrigger::Creation,
                &packet("base-a", "head-a"),
            )
            .unwrap();
        assert!(matches!(stored, PrSourceCapturePersistOutcome::Stored(_)));
        assert_eq!(
            db.persist_pr_review_guide_source_capture(
                &root,
                6,
                PrSourceCaptureTrigger::Poller,
                &packet("base-b", "head-b")
            )
            .unwrap(),
            PrSourceCapturePersistOutcome::IgnoredStaleObservation
        );
        let latest = db.get_latest_pr_review_guide_source_capture(&root).unwrap().unwrap();
        assert_eq!(latest.observation_sequence, 7);
        assert_eq!(latest.packet.head_sha, "head-a");
        assert!(latest.complete);
    }

    #[test]
    fn duplicate_endpoints_reuse_the_immutable_packet() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "deduplicate source packet");
        let first = db
            .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet("base", "head"))
            .unwrap();
        let second = db
            .persist_pr_review_guide_source_capture(
                &root,
                2,
                PrSourceCaptureTrigger::Completion,
                &packet("base", "head"),
            )
            .unwrap();
        let PrSourceCapturePersistOutcome::Stored(first) = first else {
            panic!("first capture must persist")
        };
        let PrSourceCapturePersistOutcome::Existing(second) = second else {
            panic!("same endpoints must reuse the original immutable packet")
        };
        assert_eq!(first.packet_hash, second.packet_hash);
        assert_eq!(second.observation_sequence, 1);
    }

    fn incomplete_packet(base: &str, head: &str, reason: &str) -> SourcePacket {
        let mut packet = packet(base, head);
        packet.files[0].after = Some(
            PinnedSource::builder()
                .repository("acme/widget")
                .sha(head)
                .path("src/lib.rs")
                .omission(reason)
                .build(),
        );
        packet.omissions = vec![SourceOmission {
            path: Some("src/lib.rs".to_owned()),
            side: Some(SourceSide::After),
            reason: reason.to_owned(),
        }];
        packet
    }

    #[test]
    fn incomplete_packet_is_upgraded_when_a_later_collection_is_complete() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "upgrade incomplete packet");
        let incomplete = incomplete_packet("base", "head", "pinned source read failed: timeout");
        assert!(!incomplete.is_complete());
        let first = db
            .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &incomplete)
            .unwrap();
        assert!(matches!(first, PrSourceCapturePersistOutcome::Stored(_)));
        let complete = packet("base", "head");
        let upgraded = db
            .persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &complete)
            .unwrap();
        let PrSourceCapturePersistOutcome::Stored(upgraded) = upgraded else {
            panic!("incomplete comparison must be replaced by a complete packet")
        };
        assert!(upgraded.complete);
        assert_eq!(upgraded.observation_sequence, 1);
        assert_eq!(
            db.get_latest_pr_review_guide_source_capture(&root)
                .unwrap()
                .unwrap()
                .packet
                .files[0]
                .after
                .as_ref()
                .unwrap()
                .content
                .as_deref(),
            Some("after\n")
        );
    }

    #[test]
    fn incomplete_packet_stays_sticky_when_a_retry_is_not_better() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "sticky incomplete packet");
        let first_packet = incomplete_packet("base", "head", "pinned source read failed: timeout");
        db.persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &first_packet)
            .unwrap();
        let mut worse = first_packet.clone();
        worse.omissions.push(SourceOmission {
            path: Some("src/lib.rs".to_owned()),
            side: Some(SourceSide::Before),
            reason: "second hole".to_owned(),
        });
        let reused = db
            .persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &worse)
            .unwrap();
        let PrSourceCapturePersistOutcome::Existing(existing) = reused else {
            panic!("a worse incomplete retry must keep the original packet")
        };
        assert_eq!(existing.packet.omissions.len(), 1);
        assert!(!existing.complete);
    }

    #[test]
    fn returning_to_an_earlier_comparison_selects_it_again() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "reselect earlier comparison");
        db.persist_pr_review_guide_source_capture(
            &root,
            1,
            PrSourceCaptureTrigger::Creation,
            &packet("base-a", "head-a"),
        )
        .unwrap();
        db.persist_pr_review_guide_source_capture(
            &root,
            2,
            PrSourceCaptureTrigger::Poller,
            &packet("base-b", "head-b"),
        )
        .unwrap();
        assert_eq!(
            db.get_latest_pr_review_guide_source_capture(&root)
                .unwrap()
                .unwrap()
                .packet
                .head_sha,
            "head-b"
        );
        db.persist_pr_review_guide_source_capture(
            &root,
            3,
            PrSourceCaptureTrigger::Poller,
            &packet("base-a", "head-a"),
        )
        .unwrap();
        let latest = db.get_latest_pr_review_guide_source_capture(&root).unwrap().unwrap();
        assert_eq!(latest.packet.head_sha, "head-a");
        assert_eq!(latest.observation_sequence, 1);
    }

    #[test]
    fn missing_packet_artifact_is_an_explicit_source_failure() {
        let (dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "missing artifact");
        let stored = db
            .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet("base", "head"))
            .unwrap();
        let PrSourceCapturePersistOutcome::Stored(stored) = stored else {
            panic!("capture must persist")
        };
        let relative = stored.packet_path.expect("artifact path must be recorded");
        fs::remove_file(dir.path().join(&relative)).unwrap();
        let err = db
            .get_latest_pr_review_guide_source_capture(&root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing referenced source packet blob"), "{err}");
    }

    #[test]
    fn valid_json_with_modified_metadata_fails_integrity_validation() {
        let (dir, db) = open_db();
        let original = packet("base", "head");
        db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &original)
            .unwrap();
        let capture = db.get_latest_pr_review_guide_source_capture("root").unwrap().unwrap();
        let mut modified = original.clone();
        modified.title = "corrupted metadata".to_owned();
        fs::write(
            dir.path().join(capture.packet_path.unwrap()),
            serde_json::to_vec(&modified).unwrap(),
        )
        .unwrap();
        assert!(
            db.get_latest_pr_review_guide_source_capture("root")
                .unwrap_err()
                .to_string()
                .contains("integrity failure")
        );
        assert!(
            db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Poller, &original)
                .unwrap_err()
                .to_string()
                .contains("integrity failure")
        );
    }

    #[test]
    fn diagnostic_read_selects_the_latest_pr_series() {
        let (_dir, db) = open_db();
        let first = packet("base", "first");
        let mut second = packet("base", "second");
        second.canonical_pr_url = "https://github.com/acme/widget/pull/12".to_owned();
        second.pr_number = 12;
        db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &first)
            .unwrap();
        db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Creation, &second)
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute("UPDATE pr_review_guide_source_series SET updated_at = CASE canonical_pr_url WHEN ?1 THEN '2026-01-01' ELSE '2026-01-02' END", [&first.canonical_pr_url]).unwrap();
        drop(conn);
        assert_eq!(
            db.get_latest_pr_review_guide_source_capture("root")
                .unwrap()
                .unwrap()
                .packet,
            second
        );
    }

    #[test]
    fn concurrent_same_digest_publications_are_complete() {
        let directory = tempfile::tempdir().unwrap();
        let packet = packet("base", "head");
        let bytes = serde_json::to_vec(&packet).unwrap();
        let hash = packet.content_hash().unwrap();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    let relative = publish_packet_artifact(directory.path(), &hash, &bytes).unwrap();
                    assert_eq!(load_packet(directory.path(), Some(&relative), &hash).unwrap(), packet);
                });
            }
        });
    }

    #[test]
    fn complete_reselection_fences_a_delayed_collection() {
        let (_dir, db) = open_db();
        let a = packet("base-a", "head-a");
        let b = packet("base-b", "head-b");
        db.persist_pr_review_guide_source_capture("root", 1, PrSourceCaptureTrigger::Creation, &a)
            .unwrap();
        db.persist_pr_review_guide_source_capture("root", 2, PrSourceCaptureTrigger::Poller, &b)
            .unwrap();
        assert!(
            db.select_complete_pr_review_guide_source_capture("root", &a.canonical_pr_url, "base-a", "head-a", 4)
                .unwrap()
        );
        assert_eq!(
            db.persist_pr_review_guide_source_capture("root", 3, PrSourceCaptureTrigger::Poller, &b)
                .unwrap(),
            PrSourceCapturePersistOutcome::IgnoredStaleObservation
        );
        assert_eq!(
            db.get_latest_pr_review_guide_source_capture("root")
                .unwrap()
                .unwrap()
                .packet,
            a
        );
    }

    #[test]
    fn capture_failure_is_durable_and_monotonic() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "record source failure");
        assert!(
            db.record_pr_review_guide_source_capture_failure(
                &root,
                "https://github.com/acme/widget/pull/15",
                4,
                "GitHub timed out"
            )
            .unwrap()
        );
        assert!(
            !db.record_pr_review_guide_source_capture_failure(
                &root,
                "https://github.com/acme/widget/pull/15",
                3,
                "older error"
            )
            .unwrap()
        );
    }

    #[test]
    fn source_observation_sequence_is_monotonic_before_collection() {
        let (_dir, db) = open_db();
        assert_eq!(db.allocate_pr_review_guide_source_observation_sequence().unwrap(), 1);
        assert_eq!(db.allocate_pr_review_guide_source_observation_sequence().unwrap(), 2);
    }

    #[test]
    fn revision_execution_resolves_to_the_canonical_pr_root() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "root implementation");
        let pr_url = "https://github.com/acme/widget/pull/19";
        db.update_work_item(
            &root,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr_url.to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(root.clone())
                    .description("address source feedback")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id)
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        assert_eq!(db.review_guide_source_root_for_execution(&execution.id).unwrap(), root);
    }

    fn artifact_count(root: &Path) -> usize {
        let dir = root.join(PACKET_ARTIFACT_DIR);
        let Ok(shards) = fs::read_dir(dir) else {
            return 0;
        };
        shards
            .flatten()
            .filter_map(|shard| fs::read_dir(shard.path()).ok())
            .flat_map(|files| files.flatten())
            .filter(|file| !file.file_name().to_string_lossy().ends_with(".tmp"))
            .count()
    }

    #[test]
    fn stale_observation_does_not_write_an_unreferenced_blob() {
        let (dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "stale blob");
        db.persist_pr_review_guide_source_capture(
            &root,
            7,
            PrSourceCaptureTrigger::Creation,
            &packet("base-a", "head-a"),
        )
        .unwrap();
        assert_eq!(artifact_count(dir.path()), 1);
        db.persist_pr_review_guide_source_capture(
            &root,
            6,
            PrSourceCaptureTrigger::Poller,
            &packet("base-b", "head-b"),
        )
        .unwrap();
        assert_eq!(
            artifact_count(dir.path()),
            1,
            "a rejected stale observation must not leave an unreferenced packet blob"
        );
    }

    #[test]
    fn upgrading_a_comparison_garbage_collects_the_superseded_blob() {
        let (dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "gc upgraded blob");
        let incomplete = incomplete_packet("base", "head", "pinned source read failed: timeout");
        db.persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &incomplete)
            .unwrap();
        assert_eq!(artifact_count(dir.path()), 1);
        db.persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &packet("base", "head"))
            .unwrap();
        assert_eq!(
            artifact_count(dir.path()),
            1,
            "the superseded incomplete packet blob must be deleted"
        );
        let latest = db.get_latest_pr_review_guide_source_capture(&root).unwrap().unwrap();
        assert!(latest.complete);
        assert!(latest.omission_summary.as_deref().unwrap().contains("[]"));
    }

    #[test]
    fn terminal_symlink_omission_is_stored_as_complete() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "settled symlink");
        let packet = incomplete_packet(
            "base",
            "head",
            "pinned tree entry is a symlink; omitted so Contents API cannot follow it and mis-attribute the target's bytes",
        );
        assert!(
            packet.is_complete(),
            "a structurally impossible omission must settle the packet"
        );
        let stored = db
            .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet)
            .unwrap();
        let PrSourceCapturePersistOutcome::Stored(stored) = stored else {
            panic!("settled packet must persist");
        };
        assert!(stored.complete);
        assert!(
            db.select_complete_pr_review_guide_source_capture(&root, &packet.canonical_pr_url, "base", "head", 2)
                .unwrap(),
            "a settled omission must short-circuit later poller collections"
        );
    }
}
