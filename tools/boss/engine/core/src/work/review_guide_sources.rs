//! Durable source-packet capture for automatic PR review guides.
//!
//! Core owns series identity and observation ordering. The lower
//! `boss_pr_review_sources` crate owns packet construction and reference
//! validation, so database reconciliation never grows a second GitHub client.

use super::query_ensure::RequireRow;
use super::*;
use boss_pr_review_sources::SourcePacket;

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
    pub series_id: String,
    pub root_task_id: String,
    pub observation_sequence: i64,
    pub trigger: String,
    pub packet_hash: String,
    pub complete: bool,
    pub captured_at: String,
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
            packet_json TEXT NOT NULL,
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
        let packet_json = serde_json::to_string(packet).context("serialize captured PR source packet")?;
        let packet_hash = packet.content_hash()?;
        let complete = packet.is_complete();
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
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
                    "INSERT INTO pr_review_guide_source_series
                     (id, root_task_id, canonical_pr_url, latest_observation_sequence, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 0, ?4, ?4)",
                    params![series_id, root_task_id, packet.canonical_pr_url, now],
                )?;
                (series_id, 0)
            }
        };
        if observation_sequence < latest_sequence {
            tx.commit()?;
            return Ok(PrSourceCapturePersistOutcome::IgnoredStaleObservation);
        }

        let existing = read_capture_by_endpoints(&tx, &series_id, &packet.observed_base_sha, &packet.head_sha)?;
        if let Some(existing) = existing {
            tx.execute(
                "UPDATE pr_review_guide_source_series
                 SET latest_observation_sequence = MAX(latest_observation_sequence, ?2),
                     last_capture_error = NULL,
                     updated_at = ?3
                 WHERE id = ?1",
                params![series_id, observation_sequence, now],
            )?;
            tx.commit()?;
            return Ok(PrSourceCapturePersistOutcome::Existing(existing));
        }

        let capture = PrReviewGuideSourceCapture {
            series_id: series_id.clone(),
            root_task_id: root_task_id.to_owned(),
            observation_sequence,
            trigger: trigger.as_str().to_owned(),
            packet_hash: packet_hash.clone(),
            complete,
            captured_at: now.clone(),
            packet: packet.clone(),
        };
        tx.execute(
            "INSERT INTO pr_review_guide_source_comparisons
             (id, series_id, observation_sequence, observed_base_sha, merge_base_sha, head_sha,
              trigger, packet_hash, complete, packet_json, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                next_id("prgc"),
                series_id,
                observation_sequence,
                packet.observed_base_sha,
                packet.merge_base_sha,
                packet.head_sha,
                trigger.as_str(),
                packet_hash,
                if complete { 1 } else { 0 },
                packet_json,
                now,
            ],
        )?;
        tx.execute(
            "UPDATE pr_review_guide_source_series
             SET latest_observation_sequence = ?2, last_capture_error = NULL, updated_at = ?3
             WHERE id = ?1",
            params![capture.series_id, observation_sequence, capture.captured_at],
        )?;
        tx.commit()?;
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
        let tx = conn.transaction()?;
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
                    "INSERT INTO pr_review_guide_source_series
                     (id, root_task_id, canonical_pr_url, latest_observation_sequence, last_capture_error, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 0, NULL, ?4, ?4)",
                    params![series_id, root_task_id, canonical_pr_url, now],
                )?;
                (series_id, 0)
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

    /// Diagnostic read path for an immutable packet. Returns the newest
    /// captured comparison for the requested canonical root; packet content
    /// remains in engine storage and is never re-fetched from a moving ref.
    pub fn get_latest_pr_review_guide_source_capture(
        &self,
        root_task_id: &str,
    ) -> Result<Option<PrReviewGuideSourceCapture>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT s.id, s.root_task_id, c.observation_sequence, c.trigger, c.packet_hash,
                    c.complete, c.captured_at, c.packet_json
             FROM pr_review_guide_source_series s
             JOIN pr_review_guide_source_comparisons c ON c.series_id = s.id
             WHERE s.root_task_id = ?1
             ORDER BY c.observation_sequence DESC, c.captured_at DESC
             LIMIT 1",
            [root_task_id],
            map_capture,
        )
        .optional()
        .map_err(Into::into)
    }
}

fn read_capture_by_endpoints(
    conn: &Connection,
    series_id: &str,
    base_sha: &str,
    head_sha: &str,
) -> Result<Option<PrReviewGuideSourceCapture>> {
    conn.query_row(
        "SELECT s.id, s.root_task_id, c.observation_sequence, c.trigger, c.packet_hash,
                c.complete, c.captured_at, c.packet_json
         FROM pr_review_guide_source_series s
         JOIN pr_review_guide_source_comparisons c ON c.series_id = s.id
         WHERE c.series_id = ?1 AND c.observed_base_sha = ?2 AND c.head_sha = ?3",
        params![series_id, base_sha, head_sha],
        map_capture,
    )
    .optional()
    .map_err(Into::into)
}

fn map_capture(row: &Row<'_>) -> rusqlite::Result<PrReviewGuideSourceCapture> {
    let packet_json: String = row.get(7)?;
    let packet = serde_json::from_str(&packet_json)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(error)))?;
    Ok(PrReviewGuideSourceCapture {
        series_id: row.get(0)?,
        root_task_id: row.get(1)?,
        observation_sequence: row.get(2)?,
        trigger: row.get(3)?,
        packet_hash: row.get(4)?,
        complete: row.get::<_, i64>(5)? != 0,
        captured_at: row.get(6)?,
        packet,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_active_chore, create_product, open_db};
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_pr_review_sources::{ChangeKind, PinnedSource, SourceFile};
    use boss_protocol::{CreateExecutionInput, CreateRevisionInput, ExecutionKind, ExecutionStatus, WorkItemPatch};

    fn packet(base: &str, head: &str) -> SourcePacket {
        SourcePacket {
            schema_version: 1,
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
}
