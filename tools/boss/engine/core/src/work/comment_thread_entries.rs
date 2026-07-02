//! `comment_thread_entries` persistence — engine-authored (and, in a later
//! phase, operator-authored) turns in a comment's thread, shared by the
//! bucket-1&3 nudge and the bucket-2 answer/follow-up paths (P3b of
//! `comment-triggered-document-revisions.md` §"Reply/link mechanics").
//!
//! P3b only ever writes `entry_kind = 'answer'` rows (via
//! [`WorkDb::create_comment_thread_entry`], called from the
//! `CommentsPostAnswer` handler and from `finalize_answer_agent`'s
//! no-reply-posted path). `nudge` (phase 2b) and `operator_followup` (phase
//! 3c) reuse the same table and constructor.

use super::*;

impl WorkDb {
    /// Column list for every `comment_thread_entries` SELECT. Order must
    /// match [`map_comment_thread_entry`].
    fn comment_thread_entry_columns() -> &'static str {
        "id, comment_id, entry_kind, author, body, revise_task_id, answer_agent_run_id, created_at"
    }

    /// Append a thread entry to a comment. `entry_kind` must be one of
    /// `nudge` / `answer` / `operator_followup`
    /// ([`boss_protocol::THREAD_ENTRY_KIND_NUDGE`] et al.). Unvalidated
    /// against comment state — callers own the state-machine guard (e.g.
    /// `CommentsPostAnswer` only calls this after confirming a `running`
    /// answer-agent run exists for the comment).
    pub fn create_comment_thread_entry(
        &self,
        comment_id: &str,
        entry_kind: &str,
        author: &str,
        body: &str,
        revise_task_id: Option<&str>,
        answer_agent_run_id: Option<&str>,
    ) -> Result<CommentThreadEntry> {
        match entry_kind {
            boss_protocol::THREAD_ENTRY_KIND_NUDGE
            | boss_protocol::THREAD_ENTRY_KIND_ANSWER
            | boss_protocol::THREAD_ENTRY_KIND_OPERATOR_FOLLOWUP => {}
            other => bail!("invalid comment thread entry_kind: {other}"),
        }
        let conn = self.connect()?;
        let id = next_id("cte");
        let now = now_string();
        conn.execute(
            "INSERT INTO comment_thread_entries \
             (id, comment_id, entry_kind, author, body, revise_task_id, answer_agent_run_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                comment_id,
                entry_kind,
                author,
                body,
                revise_task_id,
                answer_agent_run_id,
                now
            ],
        )?;
        let cols = Self::comment_thread_entry_columns();
        let sql = format!("SELECT {cols} FROM comment_thread_entries WHERE id = ?1");
        conn.query_row(&sql, [&id], map_comment_thread_entry)
            .map_err(Into::into)
    }

    /// List a comment's thread entries in chronological order. Not yet
    /// consumed by any handler in P3b (no thread-read RPC exists until the
    /// UI phase wires `CommentsList` to include them) — added now so the
    /// table has symmetric CRUD from day one.
    pub fn list_comment_thread_entries(&self, comment_id: &str) -> Result<Vec<CommentThreadEntry>> {
        let conn = self.connect()?;
        let cols = Self::comment_thread_entry_columns();
        let sql =
            format!("SELECT {cols} FROM comment_thread_entries WHERE comment_id = ?1 ORDER BY created_at ASC, id ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([comment_id], map_comment_thread_entry)?;
        collect_rows(rows)
    }

    /// Post an `entry_kind='nudge'` thread entry on `comment_id`. Called
    /// immediately on `directive`/`larger_change` classification (design §
    /// "Buckets 1 & 3 — unified"); `revise_task_id` starts `NULL` and is
    /// filled in later, once a `[Revise]` batch actually claims the comment.
    pub fn create_nudge_thread_entry(&self, comment_id: &str, body: &str) -> Result<CommentThreadEntry> {
        self.create_comment_thread_entry(
            comment_id,
            boss_protocol::THREAD_ENTRY_KIND_NUDGE,
            boss_protocol::THREAD_ENTRY_AUTHOR_ENGINE,
            body,
            None,
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::work::WorkDb;
    use boss_protocol::{CommentAnchor, CreateCommentInput, THREAD_ENTRY_KIND_ANSWER};
    use std::path::PathBuf;

    fn mem_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

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
    fn create_and_list_answer_entry() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let run = db
            .create_answer_agent_run(&comment, "work_item", "t1", "v0", 0)
            .unwrap();
        let entry = db
            .create_comment_thread_entry(
                &comment,
                THREAD_ENTRY_KIND_ANSWER,
                "engine",
                "The retry backoff is exponential because…",
                None,
                Some(&run.id),
            )
            .unwrap();
        assert_eq!(entry.comment_id, comment);
        assert_eq!(entry.entry_kind, THREAD_ENTRY_KIND_ANSWER);
        assert_eq!(entry.author, "engine");
        assert_eq!(entry.answer_agent_run_id.as_deref(), Some(run.id.as_str()));
        assert!(entry.revise_task_id.is_none());

        let listed = db.list_comment_thread_entries(&comment).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, entry.id);
    }

    #[test]
    fn rejects_unknown_entry_kind() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        assert!(
            db.create_comment_thread_entry(&comment, "bogus", "engine", "x", None, None)
                .is_err()
        );
    }

    #[test]
    fn create_nudge_entry_round_trips() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let entry = db
            .create_nudge_thread_entry(
                &comment,
                "This looks like it wants a doc change — click [Revise] to start one.",
            )
            .unwrap();
        assert_eq!(entry.comment_id, comment);
        assert_eq!(entry.entry_kind, "nudge");
        assert_eq!(entry.author, "engine");
        assert!(entry.revise_task_id.is_none());
        assert!(entry.answer_agent_run_id.is_none());

        let entries = db.list_comment_thread_entries(&comment).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, entry.id);
    }

    #[test]
    fn nudge_entries_list_oldest_first() {
        let db = mem_db();
        let comment = make_comment(&db, "t1");
        let first = db.create_nudge_thread_entry(&comment, "first").unwrap();
        let second = db.create_nudge_thread_entry(&comment, "second").unwrap();

        let entries = db.list_comment_thread_entries(&comment).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, first.id);
        assert_eq!(entries[1].id, second.id);
    }
}
