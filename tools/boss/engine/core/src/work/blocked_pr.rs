//! Storage for the blocked-PR sweep (`crate::blocked_pr_sweep`): the
//! per-card diagnosis the Review column renders, the remediation ledger
//! that bounds every automatic action, and the fact-gathering query the
//! sweep feeds to `boss_blocked_pr_diagnosis::diagnose`.
//!
//! Two tables, both schema 49:
//!
//! - `pr_blocked_diagnoses` — one row per work item currently blocked or
//!   stalled, replaced on every pass and deleted when the card is healthy.
//!   `get_work_tree` projects it onto `Task::review_blocked_reason`.
//! - `pr_blocked_remediations` — append-only. One row per automatic action
//!   the engine took (or declined for budget) on a PR head: the transient
//!   CI job retries and the dead-revision re-dispatches. Budgets are
//!   counted from this table, never from memory, so an engine restart
//!   cannot reset them.

use boss_blocked_pr_diagnosis::{
    AttemptFacts, BlockedFacts, CiFacts, ConflictFacts, ExecutionFacts, HeadVerdictFacts, ReviewBatchFacts,
    ReviewFacts, RevisionFacts, RevisionOrigin, TransientRetryFacts,
};
use rusqlite::OptionalExtension;

use super::*;

/// Schema 50: the blocked-PR sweep's diagnosis projection and remediation
/// ledger.
pub(in crate::work) const BLOCKED_PR_TABLES_51: &str = "CREATE TABLE IF NOT EXISTS pr_blocked_diagnoses (
         work_item_id TEXT PRIMARY KEY,
         category     TEXT NOT NULL,
         reason       TEXT NOT NULL,
         action       TEXT NOT NULL,
         updated_at   TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS pr_blocked_remediations (
         id           INTEGER PRIMARY KEY AUTOINCREMENT,
         work_item_id TEXT NOT NULL,
         head_sha     TEXT,
         category     TEXT NOT NULL,
         signal       TEXT NOT NULL,
         target       TEXT,
         action       TEXT NOT NULL,
         outcome      TEXT NOT NULL,
         detail       TEXT,
         created_at   TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS pr_blocked_remediations_item_idx
         ON pr_blocked_remediations(work_item_id, category, created_at)";

/// Ledger category for one Buildkite job retry requested because the job
/// failed on positively identified infrastructure evidence.
pub const REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY: &str = "ci_transient_retry";
/// Ledger category for re-requesting an execution of a revision whose last
/// run died with no successor.
pub const REMEDIATION_CATEGORY_REVISION_REDISPATCH: &str = "revision_redispatch";

/// Written before the action runs, so a crash between the reservation and
/// the outcome still counts against the budget. Replaced by the real outcome.
pub const REMEDIATION_OUTCOME_PENDING: &str = "pending";
pub const REMEDIATION_OUTCOME_RETRIED: &str = "retried";
pub const REMEDIATION_OUTCOME_RETRY_FAILED: &str = "retry_failed";
/// `bk job retry` timed out after the request may already have reached
/// Buildkite. Counts against the budget; the next pass's retry-lineage walk
/// settles whether the retry exists.
pub const REMEDIATION_OUTCOME_RETRY_UNCONFIRMED: &str = "retry_unconfirmed";
pub const REMEDIATION_OUTCOME_BUDGET_EXHAUSTED: &str = "budget_exhausted";
pub const REMEDIATION_OUTCOME_REQUESTED: &str = "requested";
pub const REMEDIATION_OUTCOME_REQUEST_FAILED: &str = "request_failed";

/// One ledger entry to append.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct BlockedPrRemediationInput {
    pub work_item_id: String,
    pub head_sha: Option<String>,
    pub category: String,
    /// What was observed (`upstream_unavailable: HTTP 504`, `orphaned`).
    pub signal: String,
    /// The thing acted on: a Buildkite job id, a check name, a revision id.
    pub target: Option<String>,
    /// The command or path invoked (`bk job retry <id>`, `request_execution`).
    pub action: String,
    pub outcome: String,
    pub detail: Option<String>,
}

/// One `pr_blocked_remediations` row.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct BlockedPrRemediation {
    pub head_sha: Option<String>,
    pub signal: String,
    pub target: Option<String>,
    pub outcome: String,
    pub detail: Option<String>,
    pub created_at: String,
}

/// Review-phase `blocked_reason` values that keep a `blocked` row in the
/// Review column (mirrors `board_gesture::is_review_phase_blocked`).
const REVIEW_PHASE_BLOCKED_REASONS_SQL: &str =
    "('merge_conflict', 'ci_failure', 'ci_failure_exhausted', 'review_feedback')";

/// An `active` row (alias `t`) that is held in PendingReview: it has a bound
/// PR and no live non-review execution of its own. Whether an automated
/// reviewer is live, an admission is deferred or a batch died is for the
/// diagnosis to say; a root that an implementation or fix run resumed is not
/// held for review, so no review reason may sit on it.
const PENDING_REVIEW_HELD_SQL: &str = "(t.status = 'active'
        AND t.pr_url IS NOT NULL AND t.pr_url != ''
        AND NOT EXISTS (SELECT 1 FROM work_executions we
                        WHERE we.work_item_id = t.id AND we.kind != 'pr_review'
                          AND we.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')))";

/// The card's own `tasks` columns the diagnosis reads.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct TaskRow {
    status: String,
    blocked_reason: Option<String>,
    blocked_detail: Option<String>,
    ci_required_state: Option<String>,
    ci_required_detail: Option<String>,
    pr_mergeable_state: Option<String>,
    review_required_state: Option<String>,
    head_sha: Option<String>,
    attempts_used: i64,
    product_id: String,
}

/// Ledger outcomes that spend the dead-revision re-dispatch budget. A
/// `pending` row is a reservation whose action may not have finished.
const REDISPATCH_BUDGET_OUTCOMES: [&str; 3] = [
    REMEDIATION_OUTCOME_PENDING,
    REMEDIATION_OUTCOME_REQUESTED,
    REMEDIATION_OUTCOME_REQUEST_FAILED,
];

/// One open revision row: `(id, description, status, created_via,
/// blocked_reason, blocked_detail)`.
type RevisionRow = (String, String, String, String, Option<String>, Option<String>);

#[work_db_boundary::instrument]
impl WorkDb {
    /// Every Review-lane card with a bound PR: `in_review` rows,
    /// `blocked` rows parked on a review-phase reason, and roots held
    /// `active` (PendingReview) whether or not a reviewer is live, so a
    /// reviewer stuck in dispatch, an admission hold and a reaped batch all
    /// get a reason.
    pub fn list_blocked_pr_sweep_candidates(&self) -> Result<Vec<PendingMergeCheck>> {
        self.query_pending_merge_checks(&format!(
            "(t.status = 'in_review'
               OR (t.status = 'blocked' AND t.blocked_reason IN {REVIEW_PHASE_BLOCKED_REASONS_SQL})
               OR {PENDING_REVIEW_HELD_SQL})"
        ))
    }

    /// Delete every stored diagnosis whose card is no longer a sweep
    /// candidate (`current`), returning `(work_item_id, product_id)` for each
    /// so the caller can publish a change. `product_id` is empty when the
    /// card row is gone.
    pub fn prune_pr_blocked_diagnoses(&self, current: &[String]) -> Result<Vec<(String, String)>> {
        let conn = self.connect()?;
        let stored: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT d.work_item_id, COALESCE(t.product_id, '')
                 FROM pr_blocked_diagnoses d LEFT JOIN tasks t ON t.id = d.work_item_id",
            )?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            collect_rows(rows)?
        };
        let mut removed = Vec::new();
        for (id, product_id) in stored {
            if current.contains(&id) {
                continue;
            }
            conn.execute("DELETE FROM pr_blocked_diagnoses WHERE work_item_id = ?1", [&id])?;
            removed.push((id, product_id));
        }
        Ok(removed)
    }

    /// Replace the card's diagnosis. Returns `true` when the stored
    /// category or reason changed (so the caller can publish a change).
    pub fn upsert_pr_blocked_diagnosis(
        &self,
        work_item_id: &str,
        category: &str,
        reason: &str,
        action: &str,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT category, reason FROM pr_blocked_diagnoses WHERE work_item_id = ?1",
                [work_item_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if existing.as_ref().is_some_and(|(c, r)| c == category && r == reason) {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO pr_blocked_diagnoses (work_item_id, category, reason, action, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(work_item_id) DO UPDATE SET
                 category = excluded.category, reason = excluded.reason,
                 action = excluded.action, updated_at = excluded.updated_at",
            params![work_item_id, category, reason, action, now_string()],
        )?;
        Ok(true)
    }

    /// Drop the card's diagnosis. Returns `true` when a row was removed.
    pub fn clear_pr_blocked_diagnosis(&self, work_item_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let rows = conn.execute(
            "DELETE FROM pr_blocked_diagnoses WHERE work_item_id = ?1",
            [work_item_id],
        )?;
        Ok(rows > 0)
    }

    /// `(category, reason)` currently stored for the card, if any.
    pub fn pr_blocked_diagnosis(&self, work_item_id: &str) -> Result<Option<(String, String)>> {
        let conn = self.connect()?;
        Ok(conn
            .query_row(
                "SELECT category, reason FROM pr_blocked_diagnoses WHERE work_item_id = ?1",
                [work_item_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Append a ledger row and return its id.
    pub fn record_pr_blocked_remediation(&self, input: BlockedPrRemediationInput) -> Result<i64> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO pr_blocked_remediations
                 (work_item_id, head_sha, category, signal, target, action, outcome, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                input.work_item_id,
                input.head_sha,
                input.category,
                input.signal,
                input.target,
                input.action,
                input.outcome,
                input.detail,
                now_string(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Replace a reserved row's outcome and detail once its action finished.
    pub fn update_pr_blocked_remediation(&self, id: i64, outcome: &str, detail: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE pr_blocked_remediations SET outcome = ?2, detail = ?3 WHERE id = ?1",
            params![id, outcome, detail],
        )?;
        Ok(())
    }

    /// Ledger rows for one work item and category, newest first. `head_sha`
    /// narrows to one PR head; `target` to one job / revision.
    pub fn list_pr_blocked_remediations(
        &self,
        work_item_id: &str,
        category: &str,
        head_sha: Option<&str>,
        target: Option<&str>,
    ) -> Result<Vec<BlockedPrRemediation>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT head_sha, signal, target, outcome, detail, created_at
             FROM pr_blocked_remediations
             WHERE work_item_id = ?1 AND category = ?2
               AND (?3 IS NULL OR head_sha = ?3)
               AND (?4 IS NULL OR target = ?4)
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![work_item_id, category, head_sha, target], |row| {
            Ok(BlockedPrRemediation {
                head_sha: row.get(0)?,
                signal: row.get(1)?,
                target: row.get(2)?,
                outcome: row.get(3)?,
                detail: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Number of ledger rows whose outcome counts against a budget.
    pub fn count_pr_blocked_remediations(
        &self,
        work_item_id: &str,
        category: &str,
        head_sha: Option<&str>,
        target: Option<&str>,
        outcomes: &[&str],
    ) -> Result<u32> {
        Ok(self
            .list_pr_blocked_remediations(work_item_id, category, head_sha, target)?
            .iter()
            .filter(|r| outcomes.contains(&r.outcome.as_str()))
            .count() as u32)
    }

    /// Gather everything `boss_blocked_pr_diagnosis::diagnose` needs about
    /// one card. Also returns the PR head SHA the facts were keyed on.
    ///
    /// `review_fanout_enabled` says whether a failed review batch is replaced
    /// by batch admission (the reason text differs when it is not).
    pub fn blocked_pr_facts(
        &self,
        work_item_id: &str,
        review_fanout_enabled: bool,
    ) -> Result<(BlockedFacts, Option<String>)> {
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        // The raw row reads hold the one shared connection guard, so they are
        // scoped: every `self.*` helper below (`count_pr_blocked_remediations`,
        // `effective_ci_budget`, the attempt lookups, ...) connects again and
        // would deadlock against a guard still held here.
        let (task, mut revisions) = self.blocked_pr_raw_rows(work_item_id, now)?;
        for revision in &mut revisions {
            revision.redispatches_used = self.count_pr_blocked_remediations(
                work_item_id,
                REMEDIATION_CATEGORY_REVISION_REDISPATCH,
                None,
                Some(&revision.id),
                &REDISPATCH_BUDGET_OUTCOMES,
            )?;
        }
        self.blocked_pr_facts_from_rows(work_item_id, now, task, revisions, review_fanout_enabled)
    }

    /// The card's own `tasks` row plus its open revisions, read under one
    /// connection guard that is released on return. `redispatches_used` is
    /// left at zero for the caller to fill once the guard is gone.
    fn blocked_pr_raw_rows(&self, work_item_id: &str, now: i64) -> Result<(TaskRow, Vec<RevisionFacts>)> {
        let conn = self.connect()?;
        let task: TaskRow = conn.query_row(
            "SELECT status, blocked_reason, blocked_detail, ci_required_state, ci_required_detail,
                    pr_mergeable_state, review_required_state, pr_head_sha, COALESCE(ci_attempts_used, 0),
                    product_id
             FROM tasks WHERE id = ?1",
            [work_item_id],
            |row| {
                Ok(TaskRow {
                    status: row.get(0)?,
                    blocked_reason: row.get(1)?,
                    blocked_detail: row.get(2)?,
                    ci_required_state: row.get(3)?,
                    ci_required_detail: row.get(4)?,
                    pr_mergeable_state: row.get(5)?,
                    review_required_state: row.get(6)?,
                    head_sha: row.get(7)?,
                    attempts_used: row.get(8)?,
                    product_id: row.get(9)?,
                })
            },
        )?;

        let mut revisions = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id, description, status, created_via, blocked_reason, blocked_detail
                 FROM tasks
                 WHERE parent_task_id = ?1 AND kind = 'revision' AND deleted_at IS NULL
                   AND status IN ('todo', 'active', 'blocked')
                 ORDER BY created_at ASC",
            )?;
            let rows: Vec<RevisionRow> = stmt
                .query_map([work_item_id], |row| {
                    Ok((
                        row.get(0)?,
                        row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        row.get(2)?,
                        row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            for (id, description, rev_status, created_via, rev_reason, rev_detail) in rows {
                let latest_execution = conn
                    .query_row(
                        "SELECT id, status, CAST(created_at AS INTEGER), dispatch_wait_reason, last_error
                         FROM work_executions WHERE work_item_id = ?1
                         ORDER BY created_at DESC, id DESC LIMIT 1",
                        [&id],
                        |row| {
                            Ok(ExecutionFacts {
                                id: row.get(0)?,
                                status: row.get(1)?,
                                age_secs: now - row.get::<_, i64>(2)?,
                                dispatch_wait_reason: row.get(3)?,
                                last_error: row.get(4)?,
                            })
                        },
                    )
                    .optional()?;
                let live_execution_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM work_executions
                     WHERE work_item_id = ?1
                       AND status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')",
                    [&id],
                    |row| row.get(0),
                )?;
                revisions.push(RevisionFacts {
                    id,
                    name: description.lines().next().unwrap_or_default().trim().to_owned(),
                    status: rev_status,
                    origin: RevisionOrigin::from_created_via(&created_via),
                    blocked_reason: rev_reason,
                    blocked_detail: rev_detail,
                    latest_execution,
                    live_execution_count: live_execution_count as usize,
                    redispatches_used: 0,
                });
            }
        }
        Ok((task, revisions))
    }

    /// Assemble the diagnosis input from the raw rows plus the attempt,
    /// budget and ledger lookups, each of which takes its own connection
    /// guard. The review-batch read at the end acquires a fresh guard after
    /// the last helper has released its own.
    fn blocked_pr_facts_from_rows(
        &self,
        work_item_id: &str,
        now: i64,
        task: TaskRow,
        revisions: Vec<RevisionFacts>,
        review_fanout_enabled: bool,
    ) -> Result<(BlockedFacts, Option<String>)> {
        let TaskRow {
            status,
            blocked_reason,
            blocked_detail,
            ci_required_state,
            ci_required_detail,
            pr_mergeable_state,
            review_required_state,
            head_sha,
            attempts_used,
            product_id,
        } = task;
        let head_sha = head_sha.filter(|s| !s.is_empty());
        let failing_checks = ci_required_detail
            .as_deref()
            .and_then(|json| serde_json::from_str::<Vec<serde_json::Value>>(json).ok())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let last_transient_retry = self
            .list_pr_blocked_remediations(
                work_item_id,
                REMEDIATION_CATEGORY_CI_TRANSIENT_RETRY,
                head_sha.as_deref(),
                None,
            )?
            .into_iter()
            .next()
            .map(|row| {
                let (summary, check_name) = split_signal(&row.signal);
                // `detail` starts with the "<attempt>/<budget>" token.
                let used = row
                    .detail
                    .as_deref()
                    .and_then(|d| d.split_whitespace().next())
                    .and_then(|t| t.split_once('/'))
                    .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)));
                TransientRetryFacts {
                    check_name,
                    summary,
                    outcome: row.outcome,
                    age_secs: now - row.created_at.parse::<i64>().unwrap_or(now),
                    attempt: used.map(|(a, _)| a).unwrap_or(0),
                    budget: used
                        .map(|(_, b)| b)
                        .unwrap_or(boss_ci_log_reader::transient::TRANSIENT_RETRY_BUDGET_PER_HEAD),
                }
            });
        let auto_maintenance_enabled = self.product_auto_pr_maintenance_enabled(&product_id).unwrap_or(true);
        let active_fix_attempt = self
            .active_ci_remediation_for_work_item(work_item_id)?
            .map(|a| AttemptFacts {
                status: a.status,
                revision_task_id: a.revision_task_id,
                mechanical_rung_in_flight: None,
                age_secs: now - a.created_at.parse::<i64>().unwrap_or(now),
            });
        let suppressed_for_head = match head_sha.as_deref() {
            Some(sha) => self.is_ci_failure_suppressed(work_item_id, sha).unwrap_or(false),
            None => false,
        };
        let ci = CiFacts {
            failing_checks,
            attempts_used,
            budget: self.effective_ci_budget(work_item_id).unwrap_or(3),
            last_transient_retry,
            active_fix_attempt,
            suppressed_for_head,
            auto_maintenance_enabled,
        };
        let conflict = ConflictFacts {
            active_attempt: self
                .active_conflict_resolution_for_work_item(work_item_id)?
                .map(|c| AttemptFacts {
                    status: c.status,
                    revision_task_id: c.revision_task_id,
                    mechanical_rung_in_flight: c.mechanical_rung_in_flight,
                    age_secs: now - c.created_at.parse::<i64>().unwrap_or(now),
                }),
            auto_maintenance_enabled,
        };
        let head_redispatches_used = match head_sha.as_deref() {
            Some(sha) => self.count_pr_blocked_remediations(
                work_item_id,
                REMEDIATION_CATEGORY_REVISION_REDISPATCH,
                Some(sha),
                None,
                &REDISPATCH_BUDGET_OUTCOMES,
            )?,
            None => 0,
        };
        let admission_owed = self
            .list_tasks_awaiting_pre_merge_review_admission()?
            .iter()
            .any(|c| c.task_id == work_item_id || c.cycle_root_id == work_item_id);
        // Reviewer health does not wait for the poller to observe a head: with
        // none, the newest batch's immutable target stands in, and with no
        // batch either the card family's own live reviewers are read.
        let review_sha = match head_sha.clone() {
            Some(sha) => Some(sha),
            None => self.latest_pre_merge_batch_target(work_item_id)?,
        };
        let (latest_pre_merge_batch, head_verdict, reviewers) =
            self.review_facts_for_head(work_item_id, review_sha.as_deref().unwrap_or(""), now)?;
        let facts = BlockedFacts {
            status,
            blocked_reason,
            blocked_detail,
            ci_required_state,
            pr_mergeable_state,
            revisions,
            head_redispatches_used,
            ci,
            conflict,
            review: ReviewFacts {
                head_known: review_sha.is_some() || !reviewers.is_empty(),
                latest_pre_merge_batch,
                head_verdict,
                generation_cap: review_batches::MAX_AUTOMATIC_PRE_MERGE_BATCH_GENERATIONS,
                review_required_state,
                fanout_enabled: review_fanout_enabled,
                reviewers,
                admission_owed,
            },
        };
        Ok((facts, head_sha))
    }

    /// Target SHA of the card's newest pre-merge batch, if any.
    fn latest_pre_merge_batch_target(&self, work_item_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        Ok(conn
            .query_row(
                "SELECT target_sha FROM pr_review_batches
                 WHERE cycle_root_id = ?1 AND phase = 'pre_merge' AND target_sha IS NOT NULL
                 ORDER BY generation DESC, created_at DESC, id DESC LIMIT 1",
                [work_item_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// The newest pre-merge batch and newest review verdict for the card's
    /// **observed PR head**. Both are keyed on `sha`, so a batch or verdict
    /// from an earlier head never reads as the current one.
    fn review_facts_for_head(
        &self,
        work_item_id: &str,
        sha: &str,
        now: i64,
    ) -> Result<(Option<ReviewBatchFacts>, Option<HeadVerdictFacts>, Vec<ExecutionFacts>)> {
        let conn = self.connect()?;
        let batch = conn
            .query_row(
                "SELECT b.status, b.generation
                 FROM pr_review_batches b
                 WHERE b.cycle_root_id = ?1 AND b.phase = 'pre_merge' AND b.target_sha = ?2
                 ORDER BY b.generation DESC, b.created_at DESC, b.id DESC LIMIT 1",
                params![work_item_id, sha],
                |row| {
                    Ok(ReviewBatchFacts {
                        status: row.get(0)?,
                        generation: row.get(1)?,
                    })
                },
            )
            .optional()?;
        let outcomes = [
            review_verdicts::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
            review_verdicts::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
            review_verdicts::REVIEW_GATE_OUTCOME_REVISION_CREATION_FAILED,
            review_verdicts::REVIEW_GATE_OUTCOME_GAVE_UP,
        ]
        .iter()
        .map(|o| format!("'{o}'"))
        .collect::<Vec<_>>()
        .join(",");
        // The card plus every revision descended from it: verdicts are
        // recorded against whichever of them the review ran on.
        let family = "WITH RECURSIVE family(id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT t.id, f.depth + 1 FROM family f
                JOIN tasks t ON t.parent_task_id = f.id
                WHERE t.kind = 'revision' AND f.depth < 64
             )";
        let verdict = conn
            .query_row(
                &format!(
                    "{family}
                     SELECT v.gate_outcome,
                            v.revision_task_id IS NOT NULL
                              AND EXISTS (SELECT 1 FROM tasks r
                                          WHERE r.id = v.revision_task_id AND r.kind = 'revision'
                                            AND r.deleted_at IS NULL
                                            AND r.status IN ('todo', 'active', 'blocked'))
                     FROM pr_review_verdicts v
                     JOIN family f ON f.id = v.work_item_id
                     WHERE v.head_sha = ?2 AND v.gate_outcome IN ({outcomes})
                     ORDER BY v.created_at DESC, v.id DESC LIMIT 1"
                ),
                params![work_item_id, sha],
                |row| {
                    Ok(HeadVerdictFacts {
                        gate_outcome: row.get(0)?,
                        has_successor_revision: row.get(1)?,
                    })
                },
            )
            .optional()?;
        // A findings revision that is no longer live counts as a successor
        // only once it has delivered; the review badge owns that rule (a
        // delivered one resolves to "not reviewed", an undelivered one keeps
        // the findings).
        let verdict = match verdict {
            Some(mut v)
                if !v.has_successor_revision
                    && v.gate_outcome == review_verdicts::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS =>
            {
                let states = super::review_badge::current_head_review_states(&conn, &[work_item_id.to_owned()])?;
                v.has_successor_revision = states
                    .get(work_item_id)
                    .is_some_and(|(state, _)| *state != AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS);
                Some(v)
            }
            other => other,
        };
        // Live reviewers for this head: the members of its batch, or, with
        // no batch (a legacy single reviewer), the card family's own
        // `pr_review` executions.
        let mut stmt = conn.prepare(&format!(
            "{family}
             SELECT e.id, e.status, CAST(e.created_at AS INTEGER), e.dispatch_wait_reason, e.last_error
             FROM work_executions e
             WHERE e.kind = 'pr_review'
               AND e.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
               AND (e.id IN (SELECT m.execution_id FROM pr_review_batch_members m
                             JOIN pr_review_batches b ON b.id = m.batch_id
                             WHERE b.cycle_root_id = ?1 AND b.phase = 'pre_merge' AND b.target_sha = ?2
                               AND m.execution_id IS NOT NULL)
                    OR (NOT EXISTS (SELECT 1 FROM pr_review_batches b
                                    WHERE b.cycle_root_id = ?1 AND b.phase = 'pre_merge' AND b.target_sha = ?2)
                        AND e.work_item_id IN (SELECT id FROM family)))
             ORDER BY e.created_at ASC, e.id ASC"
        ))?;
        let reviewers = stmt
            .query_map(params![work_item_id, sha], |row| {
                Ok(ExecutionFacts {
                    id: row.get(0)?,
                    status: row.get(1)?,
                    age_secs: now - row.get::<_, i64>(2)?,
                    dispatch_wait_reason: row.get(3)?,
                    last_error: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((batch, verdict, reviewers))
    }
}

/// Ledger `signal` for a transient retry is `<summary> in <check name>`;
/// split it back for the card.
fn split_signal(signal: &str) -> (String, String) {
    match signal.rsplit_once(" in ") {
        Some((summary, check)) => (summary.to_owned(), check.to_owned()),
        None => (signal.to_owned(), String::new()),
    }
}

/// Signal string stored for a transient retry: `<summary> in <check name>`.
pub fn transient_retry_signal(summary: &str, check_name: &str) -> String {
    format!("{summary} in {check_name}")
}

/// Set `review_blocked_reason` / `review_blocked_category` on every card the
/// sweep has a stored diagnosis for. One batched query; other rows keep
/// `None`.
pub(crate) fn attach_review_blocked_reason(conn: &Connection, tasks: &mut [Task], chores: &mut [Task]) -> Result<()> {
    // A stored diagnosis shows on an `active` card only while that card is
    // still held in PendingReview; `blocked_pr_sweep` deletes the rest on
    // its next pass.
    let ids: Vec<String> = tasks
        .iter()
        .chain(chores.iter())
        .filter(|row| row.review_section.is_some() || row.status == TaskStatus::Active)
        .map(|row| row.id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = (1..=ids.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT d.work_item_id, d.category, d.reason
         FROM pr_blocked_diagnoses d JOIN tasks t ON t.id = d.work_item_id
         WHERE d.work_item_id IN ({placeholders})
           AND (t.status != 'active' OR {PENDING_REVIEW_HELD_SQL})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let found: std::collections::HashMap<String, (String, String)> = stmt
        .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok((row.get(0)?, (row.get(1)?, row.get(2)?)))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for row in tasks.iter_mut().chain(chores.iter_mut()) {
        if let Some((category, reason)) = found.get(&row.id) {
            row.review_blocked_category = Some(category.clone());
            row.review_blocked_reason = Some(reason.clone());
        }
    }
    Ok(())
}
