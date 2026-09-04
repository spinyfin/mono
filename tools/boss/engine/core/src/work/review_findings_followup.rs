//! Shared constructor for review-findings follow-up work.
//!
//! Two producers need the same row shape:
//!
//! - [`super::chain_helpers::resolve_revision_on_parent_close`] converts an
//!   already-minted `revision` in place when the origin PR merges.
//! - The review-verdict applier inserts a new follow-up when the origin is
//!   already merged at apply time (including the merge-during-apply race),
//!   without manufacturing a temporary revision that would violate
//!   "revision implies an open parent PR."
//!
//! Both paths must agree on origin provenance, kind (`followup` vs `chore`),
//! and the description rewrite, or a merged-origin finding would look
//! different depending on when the merge landed.

use super::*;

/// Description sentence the revision renderer writes and the follow-up
/// constructor rewrites. Kept in one place so a wording change cannot
/// silently desync the two producers.
pub(crate) const REVIEW_FINDINGS_REVISION_CLOSE_SENTENCE: &str =
    "Address ALL findings before finalising this revision.";
pub(crate) const REVIEW_FINDINGS_FOLLOWUP_CLOSE_SENTENCE: &str = "Address ALL findings before closing this follow-up.";

/// Field values the two producers write onto a review-findings follow-up.
#[derive(Debug, Clone)]
pub(crate) struct ReviewFindingsFollowupPlan {
    pub kind: TaskKind,
    pub origin_task_short_id: Option<i64>,
    pub origin_pr_number: Option<i64>,
    pub description: String,
}

/// Derive follow-up kind, origin provenance, and rewritten description from
/// the chain root and the revision-style instructions.
pub(crate) fn plan_review_findings_followup(
    conn: &Connection,
    chain_root_id: &str,
    description: &str,
) -> Result<ReviewFindingsFollowupPlan> {
    // Emit a `followup` with provenance when the chain root still has a
    // parseable origin PR. Only tag `Followup` when that number is
    // present — `followup_pr_body_prefix` treats the kind as a hard
    // origin-PR contract and bails at dispatch otherwise, so minting a
    // Followup with `origin_pr_number = None` (missing/soft-deleted root
    // or unparseable `pr_url`) would permanently wedge the task. Fall
    // back to the historical `chore` in that case, matching the
    // deferred-scope creation site.
    let root = query_task(conn, chain_root_id)?;
    let origin_task_short_id = root.as_ref().and_then(|r| r.short_id);
    let origin_pr_number = root
        .as_ref()
        .and_then(|r| r.pr_url.as_deref().and_then(stored_pr_number));
    let kind = if origin_pr_number.is_some() {
        TaskKind::Followup
    } else {
        TaskKind::Chore
    };
    let description = description.replace(
        REVIEW_FINDINGS_REVISION_CLOSE_SENTENCE,
        REVIEW_FINDINGS_FOLLOWUP_CLOSE_SENTENCE,
    );
    Ok(ReviewFindingsFollowupPlan {
        kind,
        origin_task_short_id,
        origin_pr_number,
        description,
    })
}

/// The one work item already materialised for this `pr_review:` created_via
/// key, if any. Includes tombstoned and converted-to-followup rows so a
/// retry remains a no-op after parent-close conversion or a merged-origin
/// insert.
pub(crate) fn existing_review_findings_work_item(conn: &Connection, created_via: &str) -> Result<Option<Task>> {
    if !created_via.starts_with(CREATED_VIA_PR_REVIEW_PREFIX) {
        return Ok(None);
    }
    let existing_id: Option<String> = conn
        .query_row(
            "SELECT id
             FROM tasks
             WHERE created_via = ?1
             ORDER BY (deleted_at IS NULL) DESC, created_at ASC, id ASC
             LIMIT 1",
            params![created_via],
            |row| row.get(0),
        )
        .optional()?;
    match existing_id {
        Some(existing_id) => query_task(conn, &existing_id)?
            .ok_or_else(|| anyhow::anyhow!("pr_review materialisation {existing_id} disappeared during dedup"))
            .map(Some),
        None => Ok(None),
    }
}

/// Convert a pending review-findings revision in place to a follow-up/chore.
/// Returns the number of rows changed (0 if the row was already resolved).
pub(crate) fn convert_revision_to_review_findings_followup(
    conn: &Connection,
    rev: &Task,
    chain_root_id: &str,
    now: &str,
    autostart: bool,
) -> Result<usize> {
    let plan = plan_review_findings_followup(conn, chain_root_id, &rev.description)?;
    let rows_changed = conn.execute(
        "UPDATE tasks
         SET project_id           = NULL,
             kind                 = ?2,
             description          = ?3,
             status               = 'todo',
             ordinal              = NULL,
             pr_url               = NULL,
             deleted_at           = NULL,
             updated_at           = ?4,
             autostart             = ?5,
             created_via          = ?6,
             parent_task_id       = NULL,
             origin_task_short_id = ?7,
             origin_pr_number     = ?8,
             blocked_reason       = NULL,
             blocked_attempt_id   = NULL,
             archived_by          = NULL,
             archived_at          = NULL,
             archived_reason      = NULL,
             merge_queue_state    = NULL,
             merge_queue_detail   = NULL,
             completed_at         = NULL,
             last_status_actor    = 'engine'
         WHERE id = ?1
           AND kind = 'revision'
           AND deleted_at IS NULL",
        params![
            rev.id,
            plan.kind.as_str(),
            plan.description,
            now,
            i64::from(autostart),
            rev.created_via,
            plan.origin_task_short_id,
            plan.origin_pr_number,
        ],
    )?;
    Ok(rows_changed)
}

/// Inputs for inserting a new review-findings follow-up. Bundled so the
/// constructor stays under clippy's argument limit while remaining the
/// single insert path the verdict applier and tests share.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct ReviewFindingsFollowupInsert {
    pub product_id: String,
    pub name: String,
    pub created_via: String,
    pub description: String,
    pub chain_root_id: String,
    #[builder(default = true)]
    pub autostart: bool,
    pub reasoning: Option<boss_protocol::ReasoningMode>,
}

/// Insert a new review-findings follow-up (or return the existing one keyed
/// on `created_via`). Used when the origin PR is already merged at apply
/// time, so no revision row is ever created.
pub(crate) fn insert_review_findings_followup(conn: &Connection, input: ReviewFindingsFollowupInsert) -> Result<Task> {
    if let Some(existing) = existing_review_findings_work_item(conn, &input.created_via)? {
        tracing::warn!(
            created_via = input.created_via.as_str(),
            existing_task_id = %existing.id,
            existing_kind = %existing.kind,
            existing_status = %existing.status,
            existing_deleted = existing.deleted_at.is_some(),
            "pr_review findings materialisation already exists; duplicate mint is a no-op",
        );
        return Ok(existing);
    }
    let plan = plan_review_findings_followup(conn, &input.chain_root_id, &input.description)?;
    insert_chore_in_tx(
        conn,
        CreateChoreInput::builder()
            .product_id(input.product_id)
            .name(input.name)
            .maybe_description(Some(plan.description))
            .created_via(input.created_via)
            .force_duplicate(true)
            .autostart(input.autostart)
            .maybe_kind_override(Some(plan.kind))
            .maybe_origin_task_short_id(plan.origin_task_short_id)
            .maybe_origin_pr_number(plan.origin_pr_number)
            .maybe_reasoning(input.reasoning)
            .build(),
    )
}

impl WorkDb {
    /// Create a review-findings follow-up against `main` for a merged (or
    /// closed) origin PR. Idempotent on `created_via`.
    pub(crate) fn create_review_findings_followup(&self, input: ReviewFindingsFollowupInsert) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = insert_review_findings_followup(&tx, input)?;
        tx.commit()?;
        Ok(task)
    }
}
