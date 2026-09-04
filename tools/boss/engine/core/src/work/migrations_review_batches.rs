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
