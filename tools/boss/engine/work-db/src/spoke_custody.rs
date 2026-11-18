//! The hub's ledger of transcript and artifact bytes it holds for spoke runs.
//!
//! `spoke_ingress` holds the original frames; this module records how much of
//! each *resource* inside those frames (one transcript generation, one
//! registered artifact, one worker output) has been written to the hub's
//! custody directory. The bytes live in files, the facts about them live here:
//!
//! * `materialized` is the contiguous prefix that is durably on disk. It moves
//!   only after the file is fsynced, so a crash between the write and this row
//!   replays the chunk and rewrites the same bytes at the same offset.
//! * `state` says whether more bytes can still arrive (`open`), the artifact is
//!   whole and verified (`complete`), the spoke declared the rest unavailable
//!   (`unavailable`), the worker's output was positively bad (`invalid`), or the
//!   hub could not trust its own copy (`halted`). A reader never has to guess
//!   which kind of "not there" it is looking at.
//!
//! Nothing here touches the filesystem and nothing decides what the evidence
//! means for work status; `boss-spoke-custody` writes the files and the
//! engine's existing consumers decide.
use crate::*;

pub(crate) const MIGRATION_ID: &str = "20261112_2330_spoke_custody";

/// Where one resource's custody stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyState {
    /// Bytes may still arrive.
    Open,
    /// A registered artifact whose every byte is present and whose digest
    /// matches its identity.
    Complete,
    /// The spoke said the rest of this resource will never arrive. The bytes
    /// already held are real; nothing is implied about the missing ones.
    Unavailable,
    /// The worker's output exists but is not usable (not JSON, too large, ...),
    /// or the artifact's bytes do not match their identity.
    Invalid,
    /// The hub's own copy cannot be trusted: a hole, a changed overlap, a
    /// shortened file. The resource stops being projected and raises an alert.
    Halted,
}

impl CustodyState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Complete => "complete",
            Self::Unavailable => "unavailable",
            Self::Invalid => "invalid",
            Self::Halted => "halted",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "complete" => Self::Complete,
            "unavailable" => Self::Unavailable,
            "invalid" => Self::Invalid,
            "halted" => Self::Halted,
            _ => Self::Open,
        }
    }
}

/// One resource the hub holds, or has been told it will not get.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct SpokeCustody {
    pub execution_id: String,
    pub assignment_generation: u64,
    /// `transcript:<generation>`, `artifact:<id>` or `output:<name>`.
    pub resource: String,
    pub host_id: String,
    pub run_id: String,
    /// `transcript`, `artifact` or `output`.
    pub kind: String,
    pub state: CustodyState,
    /// The hub file holding the bytes, relative to the custody root. Empty when
    /// no bytes were ever written.
    #[builder(default)]
    pub path: String,
    /// Contiguous bytes durably on disk.
    #[builder(default)]
    pub materialized: u64,
    /// The size the spoke declared for an artifact.
    pub total_bytes: Option<u64>,
    /// The wire `ArtifactKind` of an artifact.
    pub artifact_kind: Option<i32>,
    /// Lowercase hex SHA-256 of an artifact's whole content, from its identity.
    pub expected_digest: Option<String>,
    pub metadata_json: Option<String>,
    /// The last chunk that carried bytes reached the end of the source as it
    /// stood then. Not a promise that the source is finished.
    #[builder(default)]
    pub caught_up: bool,
    /// Why the resource is unavailable, invalid or halted.
    pub reason: Option<String>,
    /// The ingress sequence of the newest record that changed this row.
    #[builder(default)]
    pub source_sequence: u64,
    #[builder(default)]
    pub created_at_ms: i64,
    #[builder(default)]
    pub updated_at_ms: i64,
}

fn now_ms() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_ms() as i64
}

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS spoke_custody (
            execution_id          TEXT NOT NULL,
            assignment_generation INTEGER NOT NULL,
            resource              TEXT NOT NULL,
            host_id               TEXT NOT NULL,
            run_id                TEXT NOT NULL,
            kind                  TEXT NOT NULL,
            state                 TEXT NOT NULL,
            path                  TEXT NOT NULL DEFAULT '',
            materialized          INTEGER NOT NULL DEFAULT 0,
            total_bytes           INTEGER,
            artifact_kind         INTEGER,
            expected_digest       TEXT,
            metadata_json         TEXT,
            caught_up             INTEGER NOT NULL DEFAULT 0,
            reason                TEXT,
            source_sequence       INTEGER NOT NULL DEFAULT 0,
            created_at_ms         INTEGER NOT NULL,
            updated_at_ms         INTEGER NOT NULL,
            PRIMARY KEY (execution_id, assignment_generation, resource)
         );
         CREATE INDEX IF NOT EXISTS spoke_custody_kind_idx
            ON spoke_custody(execution_id, kind, source_sequence);",
    )?;
    Ok(())
}

const COLUMNS: &str = "execution_id, assignment_generation, resource, host_id, run_id, kind, state, path,
    materialized, total_bytes, artifact_kind, expected_digest, metadata_json, caught_up, reason,
    source_sequence, created_at_ms, updated_at_ms";

fn map_custody(row: &Row<'_>) -> rusqlite::Result<SpokeCustody> {
    Ok(SpokeCustody {
        execution_id: row.get(0)?,
        assignment_generation: row.get::<_, i64>(1)? as u64,
        resource: row.get(2)?,
        host_id: row.get(3)?,
        run_id: row.get(4)?,
        kind: row.get(5)?,
        state: CustodyState::parse(&row.get::<_, String>(6)?),
        path: row.get(7)?,
        materialized: row.get::<_, i64>(8)? as u64,
        total_bytes: row.get::<_, Option<i64>>(9)?.map(|n| n as u64),
        artifact_kind: row.get(10)?,
        expected_digest: row.get(11)?,
        metadata_json: row.get(12)?,
        caught_up: row.get::<_, i64>(13)? != 0,
        reason: row.get(14)?,
        source_sequence: row.get::<_, i64>(15)? as u64,
        created_at_ms: row.get(16)?,
        updated_at_ms: row.get(17)?,
    })
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// The custody row for one resource of one assignment.
    pub fn spoke_custody(
        &self,
        execution_id: &str,
        assignment_generation: u64,
        resource: &str,
    ) -> Result<Option<SpokeCustody>> {
        Ok(self
            .connect()?
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM spoke_custody
                      WHERE execution_id = ?1 AND assignment_generation = ?2 AND resource = ?3"
                ),
                params![execution_id, assignment_generation as i64, resource],
                map_custody,
            )
            .optional()?)
    }

    /// The row whose file is at `path`, relative to the custody root. How a
    /// caller holding only a recorded transcript path learns it is one of ours.
    pub fn spoke_custody_at(&self, path: &str) -> Result<Option<SpokeCustody>> {
        Ok(self
            .connect()?
            .query_row(
                &format!("SELECT {COLUMNS} FROM spoke_custody WHERE path = ?1 LIMIT 1"),
                [path],
                map_custody,
            )
            .optional()?)
    }

    /// Every resource held for an execution, newest assignment and newest
    /// record first, so a caller that wants "the latest" takes the head.
    pub fn spoke_custody_for_execution(&self, execution_id: &str) -> Result<Vec<SpokeCustody>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM spoke_custody WHERE execution_id = ?1
              ORDER BY assignment_generation DESC, source_sequence DESC, resource"
        ))?;
        let rows = statement.query_map([execution_id], map_custody)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Insert `next`, or replace the stored row when it still reads
    /// `(materialized, state) == expected`. Returns whether the write took
    /// effect: `false` means another writer moved the row first and the caller
    /// should read it again. The row only ever moves forward; a stale write
    /// cannot undo a halt.
    pub fn put_spoke_custody(&self, next: &SpokeCustody, expected: Option<(u64, CustodyState)>) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_ms();
        let generation = next.assignment_generation as i64;
        let state = next.state.as_str();
        let (materialized, total, caught_up, sequence) = (
            next.materialized as i64,
            next.total_bytes.map(|n| n as i64),
            next.caught_up as i64,
            next.source_sequence as i64,
        );
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![
            &next.execution_id,
            &generation,
            &next.resource,
            &next.host_id,
            &next.run_id,
            &next.kind,
            &state,
            &next.path,
            &materialized,
            &total,
            &next.artifact_kind,
            &next.expected_digest,
            &next.metadata_json,
            &caught_up,
            &next.reason,
            &sequence,
            &now,
            &now,
        ];
        let changed = match expected {
            None => conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO spoke_custody ({COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"
                ),
                rusqlite::params_from_iter(values),
            )?,
            Some((expected_materialized, expected_state)) => {
                let (expected_materialized, expected_state) = (expected_materialized as i64, expected_state.as_str());
                values.push(&expected_materialized);
                values.push(&expected_state);
                conn.execute(
                    "UPDATE spoke_custody
                        SET state = ?7, path = ?8, materialized = ?9, total_bytes = ?10, artifact_kind = ?11,
                            expected_digest = ?12, metadata_json = ?13, caught_up = ?14, reason = ?15,
                            source_sequence = ?16, updated_at_ms = ?18
                      WHERE execution_id = ?1 AND assignment_generation = ?2 AND resource = ?3
                        AND materialized = ?19 AND state = ?20",
                    rusqlite::params_from_iter(values),
                )?
            }
        };
        Ok(changed > 0)
    }

    /// Executions that hold custody rows, for the retention pass.
    pub fn spoke_custody_executions(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare("SELECT DISTINCT execution_id FROM spoke_custody ORDER BY execution_id")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forget an execution's custody rows once its files are reclaimed.
    pub fn forget_spoke_custody(&self, execution_id: &str) -> Result<u64> {
        Ok(self
            .connect()?
            .execute("DELETE FROM spoke_custody WHERE execution_id = ?1", [execution_id])? as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    fn row(resource: &str, materialized: u64) -> SpokeCustody {
        SpokeCustody::builder()
            .execution_id("exec-1")
            .assignment_generation(1)
            .resource(resource)
            .host_id("host-a")
            .run_id("run-1")
            .kind("transcript")
            .state(CustodyState::Open)
            .materialized(materialized)
            .build()
    }

    #[test]
    fn a_row_moves_only_from_the_state_the_writer_read() {
        let db = db();
        assert!(db.put_spoke_custody(&row("transcript:0-1", 0), None).unwrap());
        // A second insert of the same resource does not replace it.
        assert!(!db.put_spoke_custody(&row("transcript:0-1", 9), None).unwrap());
        assert_eq!(
            db.spoke_custody("exec-1", 1, "transcript:0-1")
                .unwrap()
                .unwrap()
                .materialized,
            0
        );

        assert!(
            db.put_spoke_custody(&row("transcript:0-1", 10), Some((0, CustodyState::Open)))
                .unwrap()
        );
        // The same read, presented again, is stale now.
        assert!(
            !db.put_spoke_custody(&row("transcript:0-1", 20), Some((0, CustodyState::Open)))
                .unwrap()
        );
        let stored = db.spoke_custody("exec-1", 1, "transcript:0-1").unwrap().unwrap();
        assert_eq!((stored.materialized, stored.state), (10, CustodyState::Open));
    }

    #[test]
    fn newest_record_leads_the_execution_listing() {
        let db = db();
        let mut early = row("transcript:0-1", 4);
        early.source_sequence = 3;
        let mut late = row("artifact:1:abc", 4);
        late.source_sequence = 9;
        db.put_spoke_custody(&early, None).unwrap();
        db.put_spoke_custody(&late, None).unwrap();
        let listed = db.spoke_custody_for_execution("exec-1").unwrap();
        assert_eq!(
            listed.iter().map(|r| r.resource.as_str()).collect::<Vec<_>>(),
            ["artifact:1:abc", "transcript:0-1"]
        );
        assert_eq!(db.forget_spoke_custody("exec-1").unwrap(), 2);
        assert!(db.spoke_custody_for_execution("exec-1").unwrap().is_empty());
    }
}
