use super::*;

/// The reviewer completed and produced qualifying findings — a revision was
/// warranted per the severity gate, and (barring a later
/// `revision_creation_failed` amendment) one was created.
pub const REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS: &str = "completed_with_findings";
/// The reviewer completed with no qualifying findings — a genuinely clean
/// pass, not an absence of a verdict.
pub const REVIEW_GATE_OUTCOME_COMPLETED_CLEAN: &str = "completed_clean";
/// The auto-nudge breaker tripped before the reviewer ever produced a
/// parseable `ReviewResult`; the engine gave up re-prompting it. No findings
/// were ever computed for this pass.
pub const REVIEW_GATE_OUTCOME_GAVE_UP: &str = "gave_up";
/// The reviewer produced qualifying findings, but the duplicate-head guard
/// recognized this pass reviewed a head sha a prior completed pass already
/// covered and dropped it rather than minting a redundant revision.
pub const REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD: &str = "dropped_duplicate_head";
/// The reviewer produced qualifying findings and a revision was warranted,
/// but `create_revision` failed (parent no longer revisable — PR merged or
/// closed between review and now) — the findings were computed and then
/// discarded. Amended onto the row after the initial `completed_with_findings`
/// write via [`WorkDb::mark_review_verdict_revision_creation_failed`].
pub const REVIEW_GATE_OUTCOME_REVISION_CREATION_FAILED: &str = "revision_creation_failed";

/// What [`crate::completion::WorkerCompletionHandler::finalize_pr_review_pass`]
/// knows about a completed reviewer pass at the moment it calls
/// [`WorkDb::record_worker_pr_completion`]. Written into `pr_review_verdicts`
/// in the SAME transaction as that call so a `pr_review` pass can never reach
/// `completed` without a durable verdict row, and a verdict row can never
/// exist for a pass that didn't actually complete.
///
/// `revision_task_id` is deliberately absent here — whether a revision is
/// actually created is decided by a separate `create_revision` call the
/// completion transaction cannot include (it requires the parent to already
/// have transitioned to `in_review`, which this same completion write is
/// what performs). Recorded afterward via
/// [`WorkDb::set_review_verdict_revision_task_id`] on success, or
/// [`WorkDb::mark_review_verdict_revision_creation_failed`] on failure — see
/// those methods' docs for why an amend-after-the-fact still satisfies "a
/// verdict cannot exist without its pass": the row itself is born in the
/// same transaction as the pass; only its `revision_task_id` /
/// `gate_outcome` fields the amend.
#[derive(Debug, Clone)]
pub struct ReviewVerdictInput {
    pub head_sha: Option<String>,
    pub findings_count: i64,
    /// The severity gate's own answer for this pass — did the findings
    /// warrant a revision — independent of what happened to that answer
    /// afterward. It stays `true` for both destroy paths
    /// (`dropped_duplicate_head`, `revision_creation_failed`): the gate did
    /// warrant a revision in both cases, even though none exists. Whether a
    /// revision was actually minted, suppressed as a duplicate, or lost to a
    /// failed `create_revision` is entirely encoded in `gate_outcome`; do not
    /// derive that from this field.
    pub revision_warranted: bool,
    pub gate_outcome: &'static str,
}

/// A durable per-pass review verdict row, as recorded in `pr_review_verdicts`.
/// See [`ReviewVerdictInput`] and [`WorkDb::latest_review_verdict`].
///
/// Constructed via [`map_review_verdict`]'s DB-mapper struct literal, not the
/// derived builder below — the builder exists for the (currently absent, but
/// project-convention-mandated at >5 fields) construction sites outside the
/// mapper, mirroring `Task`/`WorkExecution`.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewVerdict {
    pub id: String,
    pub execution_id: String,
    pub work_item_id: String,
    pub head_sha: Option<String>,
    pub findings_count: i64,
    /// The severity gate's own answer, independent of `gate_outcome` — see
    /// [`ReviewVerdictInput::revision_warranted`] for why it does not track
    /// whether a revision actually exists.
    pub revision_warranted: bool,
    pub gate_outcome: String,
    pub revision_task_id: Option<String>,
    pub created_at: String,
}

fn map_review_verdict(row: &rusqlite::Row) -> rusqlite::Result<ReviewVerdict> {
    Ok(ReviewVerdict {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        work_item_id: row.get(2)?,
        head_sha: row.get(3)?,
        findings_count: row.get(4)?,
        revision_warranted: row.get::<_, i64>(5)? != 0,
        gate_outcome: row.get(6)?,
        revision_task_id: row.get(7)?,
        created_at: row.get(8)?,
    })
}

impl WorkDb {
    /// Insert the durable per-pass review verdict row for `execution_id`,
    /// against an already-open transaction. Called from
    /// [`Self::record_worker_pr_completion`] so a `pr_review` pass can never
    /// reach `completed` without a verdict row committing atomically with it.
    pub(crate) fn insert_review_verdict_in_tx(
        conn: &Connection,
        execution_id: &str,
        work_item_id: &str,
        input: &ReviewVerdictInput,
    ) -> Result<()> {
        let id = next_id("rvv");
        let now = now_string();
        conn.execute(
            "INSERT INTO pr_review_verdicts (
                id, execution_id, work_item_id, head_sha, findings_count,
                revision_warranted, gate_outcome, revision_task_id, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8)",
            params![
                id,
                execution_id,
                work_item_id,
                input.head_sha,
                input.findings_count,
                input.revision_warranted as i64,
                input.gate_outcome,
                now,
            ],
        )?;
        Ok(())
    }

    /// Record that the revision a verdict predicted (`gate_outcome =
    /// completed_with_findings`) was actually created. Best-effort follow-up
    /// write, outside the completion transaction — `create_revision` cannot
    /// run inside it (see `finalize_pr_review_pass`: the create-time
    /// revisability gate is evaluated after the parent has already
    /// transitioned to `in_review`, which is what that transaction does).
    /// A failure here is logged and swallowed by the caller; it never
    /// invalidates the revision that was actually created.
    pub fn set_review_verdict_revision_task_id(&self, execution_id: &str, revision_task_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let rows = conn.execute(
            "UPDATE pr_review_verdicts SET revision_task_id = ?2 WHERE execution_id = ?1",
            params![execution_id, revision_task_id],
        )?;
        if rows == 0 {
            tracing::warn!(execution_id, revision_task_id, "review verdict amend matched no row");
        }
        Ok(())
    }

    /// Amend a verdict from `completed_with_findings` to
    /// `revision_creation_failed` when `create_revision` fails after the
    /// verdict was written — the destroy path this whole mechanism exists to
    /// catch (findings computed, then silently discarded because the parent
    /// was no longer revisable). Without this the row would keep claiming
    /// `completed_with_findings` with no revision to show for it, which reads
    /// identically to a revision that simply hasn't dispatched yet.
    pub fn mark_review_verdict_revision_creation_failed(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let rows = conn.execute(
            "UPDATE pr_review_verdicts SET gate_outcome = ?2 WHERE execution_id = ?1",
            params![execution_id, REVIEW_GATE_OUTCOME_REVISION_CREATION_FAILED],
        )?;
        if rows == 0 {
            tracing::warn!(execution_id, "review verdict amend matched no row");
        }
        Ok(())
    }

    /// The most recently recorded review verdict for `work_item_id`, or
    /// `None` if no `pr_review` pass has ever completed for it (pre-migration
    /// history, or genuinely never reviewed). Callers must treat `None` as
    /// "unknown" — never infer a clean verdict from its absence.
    pub fn latest_review_verdict(&self, work_item_id: &str) -> Result<Option<ReviewVerdict>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, execution_id, work_item_id, head_sha, findings_count,
                    revision_warranted, gate_outcome, revision_task_id, created_at
             FROM pr_review_verdicts
             WHERE work_item_id = ?1
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            params![work_item_id],
            map_review_verdict,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The review verdict recorded for a specific execution, if any. Used by
    /// tests and by the two amend paths' callers to confirm the row they
    /// expect exists before/after an amend.
    pub fn review_verdict_for_execution(&self, execution_id: &str) -> Result<Option<ReviewVerdict>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, execution_id, work_item_id, head_sha, findings_count,
                    revision_warranted, gate_outcome, revision_task_id, created_at
             FROM pr_review_verdicts
             WHERE execution_id = ?1",
            params![execution_id],
            map_review_verdict,
        )
        .optional()
        .map_err(Into::into)
    }
}
