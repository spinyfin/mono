//! Schema for `answer_agent_runs`, the per-comment read-only "mini-
//! coordinator" answer-agent run table.

use super::*;

/// Create the `answer_agent_runs` table (P3a of
/// `comment-triggered-document-revisions.md`). Tracks one ephemeral,
/// read-only "mini-coordinator" answer-agent run against a `question`-classified
/// doc comment — status, the workspace lease it held while reading code, and
/// the thread reply it produced.
///
/// Idempotent — `CREATE TABLE / INDEX IF NOT EXISTS`, safe to re-run on every
/// engine start. Deliberately parallels `magic_wand_dispatches`
/// (comment-keyed, per-run row) since both track an ephemeral LLM run against a
/// comment; the differences are the `thread_turn` / `workspace_lease_id` /
/// `reply_body` columns and the distinct `answer_agent` capability profile.
/// Timestamps are TEXT epoch-seconds, matching every other table.
pub(crate) fn migrate_answer_agent_runs_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS answer_agent_runs (
             id                 TEXT PRIMARY KEY,
             comment_id         TEXT NOT NULL REFERENCES work_comments(id),
             artifact_kind      TEXT NOT NULL,
             artifact_id        TEXT NOT NULL,
             doc_version        TEXT NOT NULL,
             thread_turn        INTEGER NOT NULL DEFAULT 0,
             status             TEXT NOT NULL,
             workspace_lease_id TEXT,
             reply_body         TEXT,
             error_kind         TEXT,
             created_at         TEXT NOT NULL,
             completed_at       TEXT,
             execution_id       TEXT REFERENCES work_executions(id) ON DELETE SET NULL
         );
         CREATE INDEX IF NOT EXISTS answer_agent_runs_by_comment
             ON answer_agent_runs(comment_id, created_at);",
    )?;
    Ok(())
}

/// Add the execution pivot to databases created before answer-agent queue
/// observability existed. Nullable so already-completed historical runs remain
/// faithfully represented rather than being assigned a guessed execution.
pub(crate) fn migrate_answer_agent_runs_execution_id_column(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "answer_agent_runs", "execution_id")? {
        conn.execute(
            "ALTER TABLE answer_agent_runs ADD COLUMN execution_id TEXT REFERENCES work_executions(id) ON DELETE SET NULL",
            [],
        )?;
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS answer_agent_runs_by_execution ON answer_agent_runs(execution_id)",
        [],
    )?;
    Ok(())
}

/// Add the `workspace_positioned` tri-state flag: `NULL` (no goto attempted —
/// the comment's target wasn't an open implementation PR, or the run predates
/// this column), `0` (goto attempted and failed, fell back to a fresh
/// checkout), `1` (goto succeeded, checkout is on the PR head). Lets the
/// guide answer prompt (`compose_guide_answer_prompt`) state truthfully
/// whether the leased checkout is actually positioned on the PR head instead
/// of assuming it whenever a source capture exists.
pub(crate) fn migrate_answer_agent_runs_workspace_positioned_column(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "answer_agent_runs", "workspace_positioned")? {
        conn.execute(
            "ALTER TABLE answer_agent_runs ADD COLUMN workspace_positioned INTEGER",
            [],
        )?;
    }
    Ok(())
}
