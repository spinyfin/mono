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

/// Gate outcomes that represent a real completed judgement about a pass —
/// the only ones the AI-review badge (`attach_ai_review_state`) will ever
/// render as `"reviewed_with_findings"` / `"reviewed_all_clear"`.
/// `gave_up` and `dropped_duplicate_head` are deliberately excluded: neither
/// is positive evidence of anything (a give-up never produced a result; a
/// dropped-duplicate pass reviewed a head an EARLIER, still-informative row
/// already covers), so a badge resolver must skip past them and keep
/// looking rather than surface either as if it were a completed verdict.
/// See [`WorkDb::latest_informative_review_verdicts`].
const INFORMATIVE_GATE_OUTCOMES: [&str; 3] = [
    REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
    REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
    REVIEW_GATE_OUTCOME_REVISION_CREATION_FAILED,
];

/// Whether `gate_outcome` is one of [`INFORMATIVE_GATE_OUTCOMES`] — the
/// same "does this outcome represent a real completed judgement" test the
/// AI-review badge resolver applies, exposed so callers outside this module
/// (currently `bossctl review show`) can classify a verdict row without
/// re-deriving the list themselves.
pub fn is_informative_gate_outcome(gate_outcome: &str) -> bool {
    INFORMATIVE_GATE_OUTCOMES.contains(&gate_outcome)
}

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

    /// Every review verdict recorded for `work_item_id`, most recent first —
    /// the full attempt history, including non-informative outcomes
    /// (`gave_up`, `dropped_duplicate_head`). Backs `bossctl review show`'s
    /// "repeated attempts" view; see [`Self::latest_informative_review_verdict`]
    /// for just the single row that answers "what's the current, load-bearing
    /// verdict."
    pub fn review_verdicts_for_work_item(&self, work_item_id: &str) -> Result<Vec<ReviewVerdict>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, execution_id, work_item_id, head_sha, findings_count,
                    revision_warranted, gate_outcome, revision_task_id, created_at
             FROM pr_review_verdicts
             WHERE work_item_id = ?1
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt
            .query_map(params![work_item_id], map_review_verdict)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The most recent verdict for `work_item_id` whose `gate_outcome` is
    /// informative (see [`is_informative_gate_outcome`]) — the single-id CLI
    /// counterpart to [`query_latest_informative_review_verdicts`], which
    /// batches this same "does this outcome count" filter across a whole
    /// board. `None` means either no pass has ever completed, or every
    /// completed pass gave up or was dropped as a duplicate head — never
    /// infer a clean result from `None`.
    pub fn latest_informative_review_verdict(&self, work_item_id: &str) -> Result<Option<ReviewVerdict>> {
        Ok(self
            .review_verdicts_for_work_item(work_item_id)?
            .into_iter()
            .find(|v| is_informative_gate_outcome(&v.gate_outcome)))
    }
}

/// Batched form of [`WorkDb::latest_review_verdict`], restricted to
/// [`INFORMATIVE_GATE_OUTCOMES`], for every id in `work_item_ids` at once.
/// Used by the AI-review badge resolver (`attach_ai_review_state`) so
/// rendering a board of cards issues one query rather than one per row; a
/// free function (rather than a `WorkDb` method) so that resolver — which
/// runs inside `get_work_tree`'s already-open connection — can call it
/// directly instead of opening a second one.
///
/// An id missing from the returned map has no informative verdict —
/// callers MUST treat that as "not reviewed yet" (no badge), never infer a
/// clean result from the absence.
pub(crate) fn query_latest_informative_review_verdicts(
    conn: &Connection,
    work_item_ids: &[String],
) -> Result<std::collections::HashMap<String, ReviewVerdict>> {
    if work_item_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let id_placeholders = work_item_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let outcome_placeholders = INFORMATIVE_GATE_OUTCOMES
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", work_item_ids.len() + i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, execution_id, work_item_id, head_sha, findings_count,
                revision_warranted, gate_outcome, revision_task_id, created_at
         FROM pr_review_verdicts
         WHERE work_item_id IN ({id_placeholders})
           AND gate_outcome IN ({outcome_placeholders})
         ORDER BY created_at ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<&dyn rusqlite::ToSql> = work_item_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    params.extend(INFORMATIVE_GATE_OUTCOMES.iter().map(|o| o as &dyn rusqlite::ToSql));
    let mut result = std::collections::HashMap::new();
    for row in stmt.query_map(params.as_slice(), map_review_verdict)? {
        let verdict = row?;
        // Ascending order means a later row for the same work item
        // overwrites an earlier one, leaving the most recent informative
        // verdict per id — same "most recent wins" contract as
        // `latest_review_verdict`.
        result.insert(verdict.work_item_id.clone(), verdict);
    }
    Ok(result)
}
