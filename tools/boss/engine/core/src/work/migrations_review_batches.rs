//! Schema migration for persisted multi-agent review batches.

use anyhow::Result;
use rusqlite::Connection;

/// Create the durable review-batch tables.
///
/// A batch freezes the target SHA and complete metadata classification before
/// any reviewer is scheduled. Members record one immutable role attempt,
/// including the driver, model, and effort selected from that snapshot. The
/// unique keys ensure one batch covers each immutable target and retries stay
/// explicit per batch role and attempt number.
pub(crate) fn migrate_pr_review_batches_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pr_review_batches (
             id                        TEXT PRIMARY KEY,
             cycle_root_id             TEXT NOT NULL,
             base_sha                  TEXT NOT NULL,
             classification_json       TEXT NOT NULL,
             created_at                TEXT NOT NULL,
             phase                     TEXT NOT NULL CHECK (phase IN ('pre_merge', 'post_merge')),
             pr_number                 INTEGER NOT NULL,
             pr_url                    TEXT NOT NULL,
             status                    TEXT NOT NULL CHECK (status IN ('collecting', 'supervising', 'applying', 'completed', 'failed')),
             target_sha                TEXT NOT NULL,
             updated_at                TEXT NOT NULL,
             completed_at              TEXT,
             final_verdict_proposal_id TEXT,
             merge_sha                 TEXT,
             UNIQUE (cycle_root_id, phase, target_sha)
         );

         CREATE INDEX IF NOT EXISTS pr_review_batches_cycle_root_idx
             ON pr_review_batches(cycle_root_id, created_at);

         CREATE TABLE IF NOT EXISTS pr_review_batch_members (
             id                 TEXT PRIMARY KEY,
             batch_id           TEXT NOT NULL REFERENCES pr_review_batches(id) ON DELETE CASCADE,
             attempt            INTEGER NOT NULL CHECK (attempt >= 1),
             created_at         TEXT NOT NULL,
             provider_effort    TEXT NOT NULL,
             requested_driver   TEXT NOT NULL,
             resolved_model     TEXT NOT NULL,
             role               TEXT NOT NULL CHECK (role IN ('claude_reviewer', 'codex_reviewer', 'grok_reviewer', 'supervisor', 'post_merge_reviewer')),
             status             TEXT NOT NULL CHECK (status IN ('pending', 'running', 'reported', 'failed')),
             updated_at         TEXT NOT NULL,
             execution_id       TEXT REFERENCES work_executions(id) ON DELETE SET NULL,
             report_proposal_id TEXT,
             terminal_at        TEXT,
             UNIQUE (batch_id, role, attempt),
             UNIQUE (execution_id)
         );

         CREATE INDEX IF NOT EXISTS pr_review_batch_members_batch_idx
             ON pr_review_batch_members(batch_id, role, attempt);",
    )?;
    Ok(())
}

/// Keep historical batches at generation one while allowing explicit pre-merge
/// re-reviews to create a separate batch. Rebuild with foreign keys disabled
/// outside the transaction so dropping the old parent cannot cascade into its
/// members. Restore enforcement on both the success and error paths.
pub(crate) fn migrate_pr_review_batch_generations(conn: &Connection) -> Result<()> {
    if super::table_has_column(conn, "pr_review_batches", "generation")? {
        return Ok(());
    }
    let foreign_keys: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let result = (|| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE pr_review_batches_next (
                 id TEXT PRIMARY KEY,
                 cycle_root_id TEXT NOT NULL,
                 base_sha TEXT NOT NULL,
                 classification_json TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 phase TEXT NOT NULL CHECK (phase IN ('pre_merge', 'post_merge')),
                 pr_number INTEGER NOT NULL,
                 pr_url TEXT NOT NULL,
                 status TEXT NOT NULL CHECK (status IN ('collecting', 'supervising', 'applying', 'completed', 'failed')),
                 target_sha TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 completed_at TEXT,
                 final_verdict_proposal_id TEXT,
                 merge_sha TEXT,
                 generation INTEGER NOT NULL DEFAULT 1 CHECK (generation >= 1),
                 CHECK (phase = 'pre_merge' OR generation = 1),
                 UNIQUE (cycle_root_id, phase, target_sha, generation)
             );
             INSERT INTO pr_review_batches_next
                 SELECT id, cycle_root_id, base_sha, classification_json, created_at,
                        phase, pr_number, pr_url, status, target_sha, updated_at,
                        completed_at, final_verdict_proposal_id, merge_sha, 1
                 FROM pr_review_batches;
             DROP TABLE pr_review_batches;
             ALTER TABLE pr_review_batches_next RENAME TO pr_review_batches;
             CREATE INDEX pr_review_batches_cycle_root_idx
                 ON pr_review_batches(cycle_root_id, created_at);",
        )?;
        let mut check = tx.prepare("PRAGMA foreign_key_check(pr_review_batch_members)")?;
        anyhow::ensure!(
            check.query([])?.next()?.is_none(),
            "review batch migration broke member foreign keys"
        );
        drop(check);
        tx.commit()?;
        Ok(())
    })();
    let restored = conn.pragma_update(None, "foreign_keys", foreign_keys);
    result?;
    restored?;
    Ok(())
}

/// Add the `explicit` provenance flag distinguishing a `bossctl review
/// start` admission from an automatic pre-merge/post-merge one. A plain
/// `ALTER TABLE ... ADD COLUMN` suffices here (unlike `generation`, which
/// needed a full rebuild for its `CHECK`/`UNIQUE` changes): `explicit` has
/// no such constraint, and defaulting existing rows to `0` correctly marks
/// every batch that predates this column as non-explicit.
pub(crate) fn migrate_pr_review_batch_explicit(conn: &Connection) -> Result<()> {
    if !super::table_has_column(conn, "pr_review_batches", "explicit")? {
        conn.execute(
            "ALTER TABLE pr_review_batches ADD COLUMN explicit INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// Stamp a batch verdict onto `pr_review_verdicts` with the proposal id as
/// the materialisation idempotency key. Legacy single-reviewer rows leave
/// both columns NULL; the unique indexes are partial so they do not
/// collide.
pub(crate) fn migrate_pr_review_verdicts_batch_columns(conn: &Connection) -> Result<()> {
    if !super::table_has_column(conn, "pr_review_verdicts", "batch_id")? {
        conn.execute("ALTER TABLE pr_review_verdicts ADD COLUMN batch_id TEXT", [])?;
    }
    if !super::table_has_column(conn, "pr_review_verdicts", "proposal_id")? {
        conn.execute("ALTER TABLE pr_review_verdicts ADD COLUMN proposal_id TEXT", [])?;
    }
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS pr_review_verdicts_batch_id_uidx
             ON pr_review_verdicts(batch_id) WHERE batch_id IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS pr_review_verdicts_proposal_id_uidx
             ON pr_review_verdicts(proposal_id) WHERE proposal_id IS NOT NULL;",
    )?;
    Ok(())
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    #[test]
    fn existing_batches_and_members_survive_generation_migration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON; CREATE TABLE work_executions (id TEXT PRIMARY KEY);")
            .unwrap();
        migrate_pr_review_batches_tables(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO work_executions VALUES ('leaf');
             INSERT INTO pr_review_batches VALUES (
                'original', 'root', 'base', '{}', '123', 'pre_merge', 42, 'pr-url',
                'completed', 'head', '456', '456', 'verdict', NULL);
             INSERT INTO pr_review_batch_members VALUES (
                'member', 'original', 1, '123', 'medium', 'claude', 'model',
                'claude_reviewer', 'reported', '456', 'leaf', 'report', '456');",
        )
        .unwrap();
        migrate_pr_review_batch_generations(&conn).unwrap();
        migrate_pr_review_batch_generations(&conn).unwrap();
        let snapshot: (i64, String, String, String, String) = conn
            .query_row(
                "SELECT b.generation, b.final_verdict_proposal_id, b.completed_at, m.execution_id, m.report_proposal_id
             FROM pr_review_batches b JOIN pr_review_batch_members m ON m.batch_id = b.id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            snapshot,
            (1, "verdict".into(), "456".into(), "leaf".into(), "report".into())
        );
        let insert = "INSERT INTO pr_review_batches
            SELECT ?1, cycle_root_id, base_sha, classification_json, created_at, ?2,
                   pr_number, pr_url, status, target_sha, updated_at, completed_at,
                   final_verdict_proposal_id, merge_sha, ?3
            FROM pr_review_batches WHERE id = 'original'";
        assert!(
            conn.execute(insert, rusqlite::params!["duplicate", "pre_merge", 1])
                .is_err()
        );
        conn.execute(insert, rusqlite::params!["next", "pre_merge", 2]).unwrap();
        assert!(
            conn.execute(insert, rusqlite::params!["duplicate-next", "pre_merge", 2])
                .is_err()
        );
        assert!(conn.execute(insert, rusqlite::params!["zero", "pre_merge", 0]).is_err());
        conn.execute(insert, rusqlite::params!["post", "post_merge", 1])
            .unwrap();
        assert!(
            conn.execute(insert, rusqlite::params!["post-next", "post_merge", 2])
                .is_err()
        );
        assert!(
            conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, bool>(0))
                .unwrap()
        );
        assert!(
            conn.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query([])
                .unwrap()
                .next()
                .unwrap()
                .is_none()
        );
        // The recreated parent still owns the original cascade relationship.
        conn.execute("DELETE FROM pr_review_batches WHERE id = 'original'", [])
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM pr_review_batch_members", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'pr_review_batches_cycle_root_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
    }
}
