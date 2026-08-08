//! `work_attachments` accessors — the metadata ledger for reviewer-visible
//! screenshot evidence.
//!
//! Bytes live outside this table, content-addressed under the engine state
//! root (`boss_engine_attachments::store`). What is stored here is everything
//! a reviewer or a retention pass needs *about* an image: who produced it,
//! for which work item, when, how big, and whether retention has since taken
//! the bytes back.
//!
//! Shaped after [`crate::work::proposals`], the sibling worker-ingress ledger:
//! replay lookup, cap counting, and insert inside one `Immediate`
//! transaction, so two concurrent `boss attach` calls from one run cannot both
//! observe "under the cap" and both insert.
//!
//! Design: `tools/boss/docs/designs/worker-screenshot-evidence-attachments.md`.

use super::*;
use boss_engine_attachments::{IngestedImage, RetainedAttachment};
use boss_protocol::{
    ATTACHMENT_CAP_PER_EXECUTION, ATTACHMENT_CAP_PER_WORK_ITEM, AttachmentMediaType, ProposalErrorCode,
    ProposalSubmissionError, WorkAttachment,
};

/// One `SubmitAttachment` write, after the handler has attributed the caller
/// and the store has validated and durably written the bytes.
///
/// Deliberately not constructible from worker-supplied strings alone:
/// `execution_id` comes from the socket peer and `image` from the store's
/// own inspection of the file.
pub struct SubmitAttachmentInput<'a> {
    /// The execution the socket peer resolved to.
    pub execution_id: &'a str,
    /// The execution's work item, denormalised so the reviewer-facing listing
    /// needs no join.
    pub work_item_id: &'a str,
    pub image: &'a IngestedImage,
    /// Worker-supplied description; empty when none was given.
    pub caption: &'a str,
}

/// What [`WorkDb::submit_work_attachment`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitAttachmentOutcome {
    pub attachment: WorkAttachment,
    /// `true` when this execution had already stored these exact bytes and
    /// the call returned the existing row untouched. A success either way.
    pub already_stored: bool,
}

const SELECT_WORK_ATTACHMENT: &str = "SELECT id, execution_id, work_item_id, caption, content_digest,
            media_type, pixel_width, pixel_height, size_bytes, source_name,
            created_at, reclaimed_at
     FROM work_attachments";

/// Map a `work_attachments` row positionally from the `SELECT` above, per the
/// DB-mapper convention: adding a column without mapping it is a compile
/// error rather than a silent omission.
///
/// `media_type` is stored as TEXT with no `CHECK` (matching every sibling
/// table's enum-as-TEXT convention). An unparseable value is a corrupt row,
/// and failing the read is deliberate — coercing it to a default would mean
/// serving a JPEG with `Content-Type: image/png`.
fn map_work_attachment(row: &Row<'_>) -> rusqlite::Result<WorkAttachment> {
    let media_raw: String = row.get(5)?;
    let media_type = media_raw
        .parse::<AttachmentMediaType>()
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, err.into()))?;
    Ok(WorkAttachment {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        work_item_id: row.get(2)?,
        caption: row.get(3)?,
        content_digest: row.get(4)?,
        media_type,
        pixel_width: row.get::<_, i64>(6)?.max(0) as u32,
        pixel_height: row.get::<_, i64>(7)?.max(0) as u32,
        size_bytes: row.get::<_, i64>(8)?.max(0) as u64,
        source_name: row.get(9)?,
        created_at: row.get(10)?,
        reclaimed_at: row.get(11)?,
    })
}

fn find_by_digest(conn: &Connection, execution_id: &str, content_digest: &str) -> Result<Option<WorkAttachment>> {
    let sql = format!("{SELECT_WORK_ATTACHMENT} WHERE execution_id = ?1 AND content_digest = ?2");
    conn.query_row(&sql, params![execution_id, content_digest], map_work_attachment)
        .optional()
        .map_err(Into::into)
}

impl WorkDb {
    /// Count what this run and this work item have already stored.
    ///
    /// Read before the store writes any bytes, so a worker at its cap is
    /// refused without a blob ever landing on disk. The transaction below
    /// re-checks; this is the cheap pre-flight, not the authority.
    pub fn attachment_counts(&self, execution_id: &str, work_item_id: &str) -> Result<(usize, usize)> {
        let conn = self.connect()?;
        count_attachments(&conn, execution_id, work_item_id)
    }

    /// Insert an attachment row, or return the existing one when this
    /// execution already stored these exact bytes.
    ///
    /// Ordering inside the transaction mirrors `submit_worker_proposal`:
    /// replay lookup first (a resubmission is not charged against the caps —
    /// a worker whose connection dropped mid-reply must not retry itself out
    /// of its own budget for work it already did), then the caps, then the
    /// insert.
    ///
    /// Returns `Err` for a storage failure and `Ok(Err(_))` for a cap
    /// refusal, which is a normal typed outcome the worker is meant to read.
    pub fn submit_work_attachment(
        &self,
        input: SubmitAttachmentInput<'_>,
    ) -> Result<std::result::Result<SubmitAttachmentOutcome, ProposalSubmissionError>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = find_by_digest(&tx, input.execution_id, &input.image.content_digest)? {
            return Ok(Ok(SubmitAttachmentOutcome {
                attachment: existing,
                already_stored: true,
            }));
        }

        let (for_execution, for_work_item) = count_attachments(&tx, input.execution_id, input.work_item_id)?;
        if let Err(refusal) = check_attachment_caps(for_execution, for_work_item) {
            return Ok(Err(refusal));
        }

        let id = next_id("atc");
        let now = now_string();
        tx.execute(
            "INSERT INTO work_attachments
                 (id, execution_id, work_item_id, caption, content_digest, media_type,
                  pixel_width, pixel_height, size_bytes, source_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                input.execution_id,
                input.work_item_id,
                input.caption,
                input.image.content_digest,
                input.image.media_type.as_str(),
                input.image.pixel_width as i64,
                input.image.pixel_height as i64,
                input.image.size_bytes as i64,
                input.image.source_name,
                now,
            ],
        )?;
        tx.commit()?;

        Ok(Ok(SubmitAttachmentOutcome {
            attachment: WorkAttachment {
                id,
                execution_id: input.execution_id.to_owned(),
                work_item_id: input.work_item_id.to_owned(),
                caption: input.caption.to_owned(),
                content_digest: input.image.content_digest.clone(),
                media_type: input.image.media_type,
                pixel_width: input.image.pixel_width,
                pixel_height: input.image.pixel_height,
                size_bytes: input.image.size_bytes,
                source_name: input.image.source_name.clone(),
                created_at: now,
                reclaimed_at: None,
            },
            already_stored: false,
        }))
    }

    /// One attachment by id, including reclaimed tombstones — the evidence
    /// server needs the tombstone to explain a dead link rather than 404.
    pub fn get_work_attachment(&self, attachment_id: &str) -> Result<Option<WorkAttachment>> {
        let conn = self.connect()?;
        let sql = format!("{SELECT_WORK_ATTACHMENT} WHERE id = ?1");
        conn.query_row(&sql, params![attachment_id], map_work_attachment)
            .optional()
            .map_err(Into::into)
    }

    /// Every attachment for a work item, newest first, across all its
    /// executions. Scoped to the work item rather than one execution because
    /// a revision chain produces several runs against one PR and the
    /// reviewer-facing question is "what evidence exists for this PR".
    pub fn list_work_attachments_for_work_item(&self, work_item_id: &str) -> Result<Vec<WorkAttachment>> {
        let conn = self.connect()?;
        let sql = format!(
            "{SELECT_WORK_ATTACHMENT}
             WHERE work_item_id = ?1
             ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![work_item_id], map_work_attachment)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Every attachment in the install, newest first. Backs
    /// `bossctl attachments list`, which reads `state.db` directly and must
    /// work when the engine is wedged.
    pub fn list_all_work_attachments(&self) -> Result<Vec<WorkAttachment>> {
        let conn = self.connect()?;
        let sql = format!("{SELECT_WORK_ATTACHMENT} ORDER BY created_at DESC, id DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_work_attachment)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Retention candidates: every not-yet-reclaimed attachment, with the
    /// liveness of its owning execution attached.
    ///
    /// Liveness comes from the execution's *status*, never from file mtime —
    /// the same rule the Codex/Grok home sweeps follow.
    pub fn list_retained_attachments(&self) -> Result<Vec<RetainedAttachment>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT a.id, a.content_digest, a.media_type, a.size_bytes, a.created_at, e.status
             FROM work_attachments a
             LEFT JOIN work_executions e ON e.id = a.execution_id
             WHERE a.reclaimed_at IS NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            let media_raw: String = row.get(2)?;
            let media_type = media_raw
                .parse::<AttachmentMediaType>()
                .map_err(|err| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, err.into()))?;
            let created_at: String = row.get(4)?;
            let status: Option<String> = row.get(5)?;
            Ok(RetainedAttachment {
                id: row.get(0)?,
                content_digest: row.get(1)?,
                media_type,
                // A row whose execution is missing (pruned out from under it)
                // is not live: nothing is going to link it again.
                is_live: status
                    .and_then(|s| s.parse::<boss_protocol::ExecutionStatus>().ok())
                    .is_some_and(|s| !s.is_terminal()),
                age_anchor_epoch: created_at.parse::<i64>().unwrap_or(0),
                size_bytes: row.get::<_, i64>(3)?.max(0) as u64,
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Every content digest still referenced by a live (non-reclaimed) row.
    /// The orphan sweep uses this to tell "a blob whose rows are all gone"
    /// from "a blob a row still points at".
    pub fn referenced_attachment_digests(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.connect()?;
        let mut stmt =
            conn.prepare("SELECT DISTINCT content_digest FROM work_attachments WHERE reclaimed_at IS NULL")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Stamp `reclaimed_at` on rows whose bytes retention has taken back.
    /// The rows survive on purpose — see the migration's doc comment.
    pub fn mark_attachments_reclaimed(&self, attachment_ids: &[String]) -> Result<usize> {
        if attachment_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connect()?;
        let now = now_string();
        let tx = conn.transaction()?;
        let mut updated = 0usize;
        {
            let mut stmt =
                tx.prepare("UPDATE work_attachments SET reclaimed_at = ?2 WHERE id = ?1 AND reclaimed_at IS NULL")?;
            for id in attachment_ids {
                updated += stmt.execute(params![id, now])?;
            }
        }
        tx.commit()?;
        Ok(updated)
    }
}

fn count_attachments(conn: &Connection, execution_id: &str, work_item_id: &str) -> Result<(usize, usize)> {
    let for_execution: i64 = conn.query_row(
        "SELECT COUNT(*) FROM work_attachments WHERE execution_id = ?1",
        params![execution_id],
        |row| row.get(0),
    )?;
    let for_work_item: i64 = conn.query_row(
        "SELECT COUNT(*) FROM work_attachments WHERE work_item_id = ?1",
        params![work_item_id],
        |row| row.get(0),
    )?;
    Ok((for_execution.max(0) as usize, for_work_item.max(0) as usize))
}

/// Refuse once either cap is reached. Caps are runaway-loop protection, not
/// scarcity — a run that legitimately needs two dozen renders is already an
/// unusually visual PR — so the message says what is going on rather than
/// inviting a retry that cannot succeed.
pub fn check_attachment_caps(
    for_execution: usize,
    for_work_item: usize,
) -> std::result::Result<(), ProposalSubmissionError> {
    if for_execution >= ATTACHMENT_CAP_PER_EXECUTION {
        return Err(ProposalSubmissionError::new(
            ProposalErrorCode::RateLimited,
            format!(
                "this run has already stored {for_execution} attachments (cap \
                 {ATTACHMENT_CAP_PER_EXECUTION}). The cap is runaway-loop protection; if you \
                 genuinely need more evidence than this, say so in the PR body instead."
            ),
        ));
    }
    if for_work_item >= ATTACHMENT_CAP_PER_WORK_ITEM {
        return Err(ProposalSubmissionError::new(
            ProposalErrorCode::RateLimited,
            format!(
                "this work item has accumulated {for_work_item} attachments across its executions \
                 (cap {ATTACHMENT_CAP_PER_WORK_ITEM})."
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use boss_engine_attachments::IngestedImage;

    fn image(digest: &str) -> IngestedImage {
        IngestedImage {
            content_digest: digest.to_owned(),
            media_type: AttachmentMediaType::Png,
            pixel_width: 800,
            pixel_height: 600,
            size_bytes: 4096,
            source_name: "shot.png".to_owned(),
        }
    }

    fn seed(db: &WorkDb) -> (String, String) {
        let product = create_test_product(db);
        let chore = create_test_chore(db, &product.id, "attachment-chore");
        let execution = create_ready_chore_execution(db, &chore.id);
        (execution.id, chore.id)
    }

    #[test]
    fn stores_and_lists_newest_first() {
        let (_dir, db) = open_db();
        let (execution_id, work_item_id) = seed(&db);

        for digest in ["aa", "bb"] {
            db.submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution_id,
                work_item_id: &work_item_id,
                image: &image(digest),
                caption: "after the fix",
            })
            .unwrap()
            .unwrap();
        }

        let listed = db.list_work_attachments_for_work_item(&work_item_id).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].caption, "after the fix");
        assert!(listed.iter().all(|a| a.reclaimed_at.is_none()));
        assert!(listed.iter().all(|a| a.id.starts_with("atc_")));
    }

    #[test]
    fn resubmitting_identical_bytes_is_a_replay_not_a_duplicate() {
        let (_dir, db) = open_db();
        let (execution_id, work_item_id) = seed(&db);

        let first = db
            .submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution_id,
                work_item_id: &work_item_id,
                image: &image("same"),
                caption: "one",
            })
            .unwrap()
            .unwrap();
        let second = db
            .submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution_id,
                work_item_id: &work_item_id,
                image: &image("same"),
                caption: "ignored on replay",
            })
            .unwrap()
            .unwrap();

        assert!(!first.already_stored);
        assert!(second.already_stored);
        assert_eq!(first.attachment.id, second.attachment.id);
        assert_eq!(db.list_work_attachments_for_work_item(&work_item_id).unwrap().len(), 1);
    }

    #[test]
    fn refuses_past_the_per_execution_cap() {
        let (_dir, db) = open_db();
        let (execution_id, work_item_id) = seed(&db);

        for n in 0..ATTACHMENT_CAP_PER_EXECUTION {
            db.submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution_id,
                work_item_id: &work_item_id,
                image: &image(&format!("digest-{n}")),
                caption: "",
            })
            .unwrap()
            .unwrap();
        }

        let refusal = db
            .submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution_id,
                work_item_id: &work_item_id,
                image: &image("one-too-many"),
                caption: "",
            })
            .unwrap()
            .expect_err("the cap must refuse, not silently drop");
        assert_eq!(refusal.code, ProposalErrorCode::RateLimited);
    }

    #[test]
    fn reclaim_marks_rows_without_deleting_them() {
        let (_dir, db) = open_db();
        let (execution_id, work_item_id) = seed(&db);
        let stored = db
            .submit_work_attachment(SubmitAttachmentInput {
                execution_id: &execution_id,
                work_item_id: &work_item_id,
                image: &image("aa"),
                caption: "",
            })
            .unwrap()
            .unwrap();

        assert_eq!(
            db.mark_attachments_reclaimed(std::slice::from_ref(&stored.attachment.id))
                .unwrap(),
            1
        );
        let after = db.get_work_attachment(&stored.attachment.id).unwrap().unwrap();
        assert!(after.is_reclaimed(), "the row survives as a tombstone");
        // Idempotent: a second sweep pass must not re-stamp or error.
        assert_eq!(db.mark_attachments_reclaimed(&[stored.attachment.id]).unwrap(), 0);
        assert!(
            db.referenced_attachment_digests().unwrap().is_empty(),
            "a reclaimed row no longer holds its blob alive"
        );
    }

    #[test]
    fn retention_candidates_carry_execution_liveness() {
        let (_dir, db) = open_db();
        let (execution_id, work_item_id) = seed(&db);
        db.submit_work_attachment(SubmitAttachmentInput {
            execution_id: &execution_id,
            work_item_id: &work_item_id,
            image: &image("aa"),
            caption: "",
        })
        .unwrap()
        .unwrap();

        let candidates = db.list_retained_attachments().unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(
            candidates[0].is_live,
            "a ready/running execution's evidence must be protected from retention"
        );

        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
                params![execution_id],
            )
            .unwrap();
        }
        assert!(!db.list_retained_attachments().unwrap()[0].is_live);
    }
}
