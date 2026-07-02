//! `answer_agent_runs` persistence — one row per ephemeral, read-only
//! "mini-coordinator" answer-agent run against a `question`-classified doc
//! comment (P3a of `comment-triggered-document-revisions.md`).
//!
//! Deliberately parallels the `magic_wand_dispatches` CRUD in
//! [`super::comments`]: a shared column list that must match
//! [`map_answer_agent_run`], an insert that re-selects the created row, and a
//! guarded completion transition (`running` → `replied`/`failed`). The table
//! is comment-keyed with a per-run row, differing from magic-wand only in the
//! `thread_turn` / `workspace_lease_id` / `reply_body` columns and the
//! distinct `answer_agent` capability profile.

use super::*;

impl WorkDb {
    /// Column list for every `answer_agent_runs` SELECT. Order must match
    /// [`map_answer_agent_run`].
    fn answer_agent_run_columns() -> &'static str {
        "id, comment_id, artifact_kind, artifact_id, doc_version, thread_turn, \
         status, workspace_lease_id, reply_body, error_kind, created_at, completed_at"
    }

    /// Insert a `running` answer-agent run row and return it. `thread_turn` is
    /// `0` for the first answer on a comment and `1+` for re-entered follow-ups.
    /// `workspace_lease_id` is `None` at creation and stamped later if/when the
    /// run leases a workspace to read code.
    pub fn create_answer_agent_run(
        &self,
        comment_id: &str,
        artifact_kind: &str,
        artifact_id: &str,
        doc_version: &str,
        thread_turn: i64,
    ) -> Result<AnswerAgentRun> {
        let conn = self.connect()?;
        let id = next_id("aar");
        let now = now_string();
        conn.execute(
            "INSERT INTO answer_agent_runs \
             (id, comment_id, artifact_kind, artifact_id, doc_version, thread_turn, \
              status, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                comment_id,
                artifact_kind,
                artifact_id,
                doc_version,
                thread_turn,
                ANSWER_AGENT_RUN_STATUS_RUNNING,
                now,
            ],
        )?;
        let cols = Self::answer_agent_run_columns();
        let sql = format!("SELECT {cols} FROM answer_agent_runs WHERE id = ?1");
        conn.query_row(&sql, [&id], map_answer_agent_run).map_err(Into::into)
    }

    /// Record the workspace lease a run acquired to check out code for reading.
    /// Idempotent-friendly: only updates a `running` row (a completed run never
    /// gains a lease). Returns the updated row.
    pub fn set_answer_agent_run_lease(&self, run_id: &str, workspace_lease_id: &str) -> Result<AnswerAgentRun> {
        let conn = self.connect()?;
        let n = conn.execute(
            "UPDATE answer_agent_runs SET workspace_lease_id = ?2 \
             WHERE id = ?1 AND status = 'running'",
            params![run_id, workspace_lease_id],
        )?;
        if n == 0 {
            bail!("answer-agent run {run_id} not found or already in a terminal state (expected running)");
        }
        let cols = Self::answer_agent_run_columns();
        let sql = format!("SELECT {cols} FROM answer_agent_runs WHERE id = ?1");
        conn.query_row(&sql, [run_id], map_answer_agent_run).map_err(Into::into)
    }

    /// Transition a run from `running` to a terminal `replied` (with the reply
    /// body) or `failed` (with an `error_kind`). Guarded on `status='running'`
    /// so a duplicate completion callback is a no-op error, not a double
    /// transition (design § "Reconciliation idempotency"). Returns the updated
    /// row.
    pub fn complete_answer_agent_run(
        &self,
        run_id: &str,
        status: &str,
        reply_body: Option<&str>,
        error_kind: Option<&str>,
    ) -> Result<AnswerAgentRun> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE answer_agent_runs \
             SET status = ?2, reply_body = ?3, error_kind = ?4, completed_at = ?5 \
             WHERE id = ?1 AND status = 'running'",
            params![run_id, status, reply_body, error_kind, now],
        )?;
        if n == 0 {
            bail!("answer-agent run {run_id} not found or already in a terminal state (expected running)");
        }
        let cols = Self::answer_agent_run_columns();
        let sql = format!("SELECT {cols} FROM answer_agent_runs WHERE id = ?1");
        conn.query_row(&sql, [run_id], map_answer_agent_run).map_err(Into::into)
    }

    /// Fetch an answer-agent run by id.
    pub fn get_answer_agent_run(&self, run_id: &str) -> Result<Option<AnswerAgentRun>> {
        let conn = self.connect()?;
        let cols = Self::answer_agent_run_columns();
        let sql = format!("SELECT {cols} FROM answer_agent_runs WHERE id = ?1");
        conn.query_row(&sql, [run_id], map_answer_agent_run)
            .optional()
            .map_err(Into::into)
    }

    /// The most recent answer-agent run for a comment (by `created_at`, then
    /// `id` as a stable tiebreak). Drives the bridging path — when a follow-up
    /// reclassifies to `directive`/`larger_change`, the latest run's
    /// `reply_body` (if any) is appended to the revision directive — and the
    /// per-comment concurrency guard (at most one live run per comment).
    pub fn latest_answer_agent_run_for_comment(&self, comment_id: &str) -> Result<Option<AnswerAgentRun>> {
        let conn = self.connect()?;
        let cols = Self::answer_agent_run_columns();
        let sql = format!(
            "SELECT {cols} FROM answer_agent_runs WHERE comment_id = ?1 \
             ORDER BY created_at DESC, id DESC LIMIT 1"
        );
        conn.query_row(&sql, [comment_id], map_answer_agent_run)
            .optional()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use crate::work::WorkDb;
    use boss_protocol::{CommentAnchor, CreateCommentInput};
    use std::path::PathBuf;

    fn mem_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

    /// Create a real `work_comments` row so the `answer_agent_runs.comment_id`
    /// foreign key (enforced under `PRAGMA foreign_keys = ON`) is satisfiable.
    fn make_comment(db: &WorkDb, artifact_id: &str) -> String {
        db.create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: artifact_id.to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "alpha".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "why does this retry three times?".to_owned(),
            author: "operator".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap()
        .id
    }

    #[test]
    fn create_and_fetch() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let run = db
            .create_answer_agent_run(&comment, "work_item", "t1", "v0", 0)
            .unwrap();
        assert_eq!(run.comment_id, comment);
        assert_eq!(run.artifact_kind, "work_item");
        assert_eq!(run.artifact_id, "t1");
        assert_eq!(run.doc_version, "v0");
        assert_eq!(run.thread_turn, 0);
        assert_eq!(run.status, "running");
        assert!(run.workspace_lease_id.is_none());
        assert!(run.reply_body.is_none());
        assert!(run.error_kind.is_none());
        assert!(run.completed_at.is_none());

        let fetched = db.get_answer_agent_run(&run.id).unwrap().unwrap();
        assert_eq!(fetched.id, run.id);
        assert_eq!(fetched.status, "running");
    }

    #[test]
    fn complete_replied() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let run = db
            .create_answer_agent_run(&comment, "work_item", "t1", "v0", 0)
            .unwrap();
        let done = db
            .complete_answer_agent_run(
                &run.id,
                "replied",
                Some("The retry backoff is exponential because…"),
                None,
            )
            .unwrap();
        assert_eq!(done.status, "replied");
        assert_eq!(
            done.reply_body.as_deref(),
            Some("The retry backoff is exponential because…")
        );
        assert!(done.error_kind.is_none());
        assert!(done.completed_at.is_some());
    }

    #[test]
    fn complete_failed() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let run = db
            .create_answer_agent_run(&comment, "work_item", "t1", "v0", 0)
            .unwrap();
        let done = db
            .complete_answer_agent_run(&run.id, "failed", None, Some("api_error"))
            .unwrap();
        assert_eq!(done.status, "failed");
        assert!(done.reply_body.is_none());
        assert_eq!(done.error_kind.as_deref(), Some("api_error"));
        assert!(done.completed_at.is_some());
    }

    #[test]
    fn complete_is_idempotency_guarded() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let run = db
            .create_answer_agent_run(&comment, "work_item", "t1", "v0", 0)
            .unwrap();
        db.complete_answer_agent_run(&run.id, "replied", Some("first"), None)
            .unwrap();
        // A duplicate completion callback finds the row terminal, so it's a
        // no-op error rather than silently re-writing the reply.
        assert!(
            db.complete_answer_agent_run(&run.id, "replied", Some("second"), None)
                .is_err()
        );
        let reloaded = db.get_answer_agent_run(&run.id).unwrap().unwrap();
        assert_eq!(reloaded.reply_body.as_deref(), Some("first"));
    }

    #[test]
    fn set_lease_only_while_running() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let run = db
            .create_answer_agent_run(&comment, "work_item", "t1", "v0", 0)
            .unwrap();
        let leased = db.set_answer_agent_run_lease(&run.id, "lease-123").unwrap();
        assert_eq!(leased.workspace_lease_id.as_deref(), Some("lease-123"));

        db.complete_answer_agent_run(&run.id, "replied", Some("done"), None)
            .unwrap();
        // A completed run never gains a lease.
        assert!(db.set_answer_agent_run_lease(&run.id, "lease-456").is_err());
    }

    #[test]
    fn latest_run_orders_by_created_then_id() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let first = db
            .create_answer_agent_run(&comment, "work_item", "t1", "v0", 0)
            .unwrap();
        let second = db
            .create_answer_agent_run(&comment, "work_item", "t1", "v1", 1)
            .unwrap();
        let latest = db.latest_answer_agent_run_for_comment(&comment).unwrap().unwrap();
        // `next_id` is monotonic, so the second insert wins even at equal
        // epoch-second `created_at`.
        assert_eq!(latest.id, second.id);
        assert_ne!(latest.id, first.id);
        assert_eq!(latest.thread_turn, 1);

        // A comment with no runs returns None.
        let other = make_comment(&db, "t2");
        assert!(db.latest_answer_agent_run_for_comment(&other).unwrap().is_none());
    }
}
