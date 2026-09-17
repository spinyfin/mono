//! Durable attempts/versions for PR review-guide generation
//! (`ExecutionKind::PrReviewGuide`), and the transactional publication fence
//! that advances the series' readable pointer.
//!
//! [`super::review_guide_sources`] owns the immutable source-packet series
//! and comparison identity, including `selected_comparison_id` — the series'
//! current desired comparison. This module owns everything downstream of
//! that: one durable attempt per generation try, one immutable version per
//! successfully validated attempt, and the fence that lets only an attempt
//! bound to the current `selected_comparison_id` and request epoch advance the series'
//! `readable_version_id`. A late-finishing attempt for a comparison the
//! series has since moved past is recorded as superseded history, never
//! published over newer content — design invariant #2.

use anyhow::ensure;
use std::sync::Arc;
use sha2::{Digest, Sha256};

use super::query_ensure::RequireRow;
use super::*;
use crate::coordinator::ExecutionPublisher;

/// Lifecycle of one `pr_review_guide_attempts` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrReviewGuideAttemptStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// Finished (or was fenced at publish time) after the series moved on to
    /// a newer desired comparison. Distinct from `Failed`: the attempt itself
    /// may have produced a perfectly valid guide — it is just for stale
    /// content, so it is retained as history rather than as an error.
    Superseded,
}

impl PrReviewGuideAttemptStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "superseded" => Ok(Self::Superseded),
            other => bail!("unknown pr_review_guide_attempts.status `{other}`"),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Superseded
        )
    }
}

/// One durable generation try, bound to exactly one immutable comparison and
/// (once dispatched) exactly one [`WorkExecution`].
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PrReviewGuideAttempt {
    pub id: String,
    pub series_id: String,
    pub comparison_id: String,
    pub request_epoch: i64,
    pub ordinal: i64,
    pub execution_id: Option<String>,
    pub status: String,
    pub prompt_version: String,
    pub driver: Option<String>,
    pub model: Option<String>,
    pub effort_value: Option<String>,
    pub error: Option<String>,
    pub retries: i64,
    pub idempotency_token: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Lossless provider usage snapshots collected through the execution hook path.
    pub provider_usage_json: Option<String>,
}

/// One immutable, validated guide version. Never mutated after insertion —
/// an explicit regenerate creates a new row rather than overwriting this one
/// (design invariant #3).
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PrReviewGuideVersion {
    pub id: String,
    pub series_id: String,
    pub comparison_id: String,
    pub attempt_id: String,
    pub markdown: String,
    pub raw_output: String,
    pub content_hash: String,
    pub prompt_version: String,
    pub generated_at: String,
}

/// Series-level summary for the `GetReviewGuide` RPC: current lifecycle plus
/// the readable version's identity (not its content — see
/// [`WorkDb::get_pr_review_guide_version`] for that).
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PrReviewGuideSummary {
    pub series_id: String,
    pub root_task_id: String,
    pub canonical_pr_url: String,
    pub lifecycle: String,
    pub request_epoch: i64,
    pub selected_comparison_id: Option<String>,
    pub readable_version_id: Option<String>,
}

/// Outcome of [`WorkDb::publish_pr_review_guide_version`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishReviewGuideOutcome {
    /// Published as the series' new readable version. Boxed: this variant
    /// carries the full immutable Markdown/raw-output content, dwarfing the
    /// unit variants below.
    Published(Box<PrReviewGuideVersion>),
    /// The series has since moved to a different desired comparison; the
    /// attempt is recorded as `superseded` history and nothing was published.
    Superseded,
    /// The attempt had already reached a terminal status (a duplicate
    /// completion signal, or a race with cancellation); no-op.
    AlreadyTerminal,
}

/// Outcome of [`WorkDb::retry_pr_review_guide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryReviewGuideOutcome {
    Created(PrReviewGuideAttempt),
    /// The same idempotency token was already used for this series; returns
    /// the original attempt rather than creating a second one.
    AlreadyRequested(PrReviewGuideAttempt),
    /// No source series/comparison exists yet for this root task — retry has
    /// nothing to regenerate from.
    NoComparison,
}

pub(crate) fn migrate_pr_review_guide_job_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pr_review_guide_attempts (
            id TEXT PRIMARY KEY,
            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),
            comparison_id TEXT NOT NULL REFERENCES pr_review_guide_source_comparisons(id),
            request_epoch INTEGER NOT NULL,
            ordinal INTEGER NOT NULL,
            execution_id TEXT,
            status TEXT NOT NULL,
            prompt_version TEXT NOT NULL,
            driver TEXT,
            model TEXT,
            effort_value TEXT,
            error TEXT,
            retries INTEGER NOT NULL DEFAULT 0,
            idempotency_token TEXT,
            created_at TEXT NOT NULL,
            started_at TEXT,
            finished_at TEXT
        );
        CREATE INDEX IF NOT EXISTS pr_review_guide_attempts_comparison_idx
            ON pr_review_guide_attempts(comparison_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS pr_review_guide_attempts_series_idx
            ON pr_review_guide_attempts(series_id, request_epoch DESC);
        CREATE INDEX IF NOT EXISTS pr_review_guide_attempts_execution_idx
            ON pr_review_guide_attempts(execution_id) WHERE execution_id IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS pr_review_guide_attempts_idempotency_idx
            ON pr_review_guide_attempts(series_id, idempotency_token)
            WHERE idempotency_token IS NOT NULL;
        CREATE TABLE IF NOT EXISTS pr_review_guide_versions (
            id TEXT PRIMARY KEY,
            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),
            comparison_id TEXT NOT NULL REFERENCES pr_review_guide_source_comparisons(id),
            attempt_id TEXT NOT NULL REFERENCES pr_review_guide_attempts(id),
            markdown TEXT NOT NULL,
            raw_output TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            prompt_version TEXT NOT NULL,
            generated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS pr_review_guide_versions_series_idx
            ON pr_review_guide_versions(series_id, generated_at DESC);",
    )?;
    if !table_has_column(conn, "pr_review_guide_attempts", "provider_usage_json")? {
        conn.execute(
            "ALTER TABLE pr_review_guide_attempts ADD COLUMN provider_usage_json TEXT",
            [],
        )?;
    }
    // Additive lifecycle columns on the series table `review_guide_sources.rs`
    // owns the CREATE for. `selected_comparison_id` (already present) IS the
    // series' desired-comparison pointer; these three columns are the only
    // guide-job state a comparison capture does not already carry.
    if !table_has_column(conn, "pr_review_guide_source_series", "guide_lifecycle")? {
        conn.execute(
            "ALTER TABLE pr_review_guide_source_series ADD COLUMN guide_lifecycle TEXT NOT NULL DEFAULT 'idle'",
            [],
        )?;
    }
    if !table_has_column(conn, "pr_review_guide_source_series", "request_epoch")? {
        conn.execute(
            "ALTER TABLE pr_review_guide_source_series ADD COLUMN request_epoch INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !table_has_column(conn, "pr_review_guide_source_series", "readable_version_id")? {
        conn.execute(
            "ALTER TABLE pr_review_guide_source_series ADD COLUMN readable_version_id TEXT",
            [],
        )?;
    }
    conn.execute_batch(
        "UPDATE work_executions SET status = 'cancelled', finished_at = datetime('now')
         WHERE status NOT IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')
           AND id IN (
             SELECT execution_id FROM pr_review_guide_attempts a
             WHERE a.status IN ('queued', 'running') AND EXISTS (
               SELECT 1 FROM pr_review_guide_attempts b WHERE b.series_id = a.series_id
                 AND b.status IN ('queued', 'running')
                 AND (b.request_epoch > a.request_epoch OR (b.request_epoch = a.request_epoch AND b.id > a.id))
             )
           );
         UPDATE pr_review_guide_attempts AS a SET status = 'superseded', finished_at = datetime('now')
         WHERE status IN ('queued', 'running') AND EXISTS (
           SELECT 1 FROM pr_review_guide_attempts b WHERE b.series_id = a.series_id
             AND b.status IN ('queued', 'running')
             AND (b.request_epoch > a.request_epoch OR (b.request_epoch = a.request_epoch AND b.id > a.id))
         );
         CREATE UNIQUE INDEX IF NOT EXISTS pr_review_guide_attempts_one_live_series
           ON pr_review_guide_attempts(series_id) WHERE status IN ('queued', 'running');",
    )?;
    Ok(())
}

impl WorkDb {
    /// Admit a request for the selected comparison, reusing a live request
    /// for that comparison or atomically replacing an obsolete request.
    /// The durable queued attempt can be dispatched by enqueue or reconcile.
    pub(crate) fn create_pr_review_guide_attempt(
        &self,
        series_id: &str,
        comparison_id: &str,
        prompt_version: &str,
    ) -> Result<PrReviewGuideAttempt> {
        self.admit_pr_review_guide_attempt(series_id, comparison_id, prompt_version, None, false)
            .map(|(attempt, _)| attempt)
    }

    #[cfg(test)]
    fn create_pr_review_guide_attempt_with_token(
        &self,
        series_id: &str,
        comparison_id: &str,
        prompt_version: &str,
        idempotency_token: Option<&str>,
    ) -> Result<PrReviewGuideAttempt> {
        self.admit_pr_review_guide_attempt(series_id, comparison_id, prompt_version, idempotency_token, true)
            .map(|(attempt, _)| attempt)
    }

    fn admit_pr_review_guide_attempt(
        &self,
        series_id: &str,
        comparison_id: &str,
        prompt_version: &str,
        idempotency_token: Option<&str>,
        replace: bool,
    ) -> Result<(PrReviewGuideAttempt, bool)> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pending = PendingEvents::new();
        if let Some(token) = idempotency_token {
            let existing = tx.query_row(
                &format!("SELECT {PR_REVIEW_GUIDE_ATTEMPT_COLUMNS} FROM pr_review_guide_attempts WHERE series_id = ?1 AND idempotency_token = ?2"),
                params![series_id, token], map_pr_review_guide_attempt,
            ).optional()?;
            if let Some(existing) = existing {
                return Ok((existing, false));
            }
        }
        let selected: Option<String> = tx.query_row(
            "SELECT selected_comparison_id FROM pr_review_guide_source_series WHERE id = ?1",
            [series_id],
            |row| row.get(0),
        )?;
        ensure!(
            selected.as_deref() == Some(comparison_id),
            "comparison is no longer selected"
        );
        let live = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {PR_REVIEW_GUIDE_ATTEMPT_COLUMNS} FROM pr_review_guide_attempts WHERE series_id = ?1 AND status IN ('queued', 'running')"
            ))?;
            stmt.query_map([series_id], map_pr_review_guide_attempt)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !replace && let Some(existing) = live.iter().find(|a| a.comparison_id == comparison_id) {
            return Ok((existing.clone(), false));
        }
        for attempt in live {
            if let Some(execution_id) = &attempt.execution_id {
                let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
                if !execution.status.is_terminal() {
                    super::executions_runs::cancel_execution_in_tx(
                        &tx,
                        &mut pending,
                        execution_id,
                        CancelExecutionOpts::default(),
                    )?;
                }
            }
            tx.execute(
                "UPDATE pr_review_guide_attempts SET status = 'superseded', error = 'replaced by a newer request', finished_at = ?2 WHERE id = ?1",
                params![attempt.id, now],
            )?;
        }
        let request_epoch: i64 = tx.query_row(
            "UPDATE pr_review_guide_source_series
             SET request_epoch = request_epoch + 1, guide_lifecycle = 'queued', updated_at = ?2
             WHERE id = ?1
             RETURNING request_epoch",
            params![series_id, now],
            |row| row.get(0),
        )?;
        let ordinal: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pr_review_guide_attempts WHERE comparison_id = ?1",
            [comparison_id],
            |row| row.get(0),
        )?;
        let attempt_id = next_id("prga");
        tx.execute(
            "INSERT INTO pr_review_guide_attempts (
                id, series_id, comparison_id, request_epoch, ordinal, execution_id, status,
                prompt_version, driver, model, effort_value, error, retries, idempotency_token,
                created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, NULL, NULL, NULL, NULL, 0, ?8, ?9, NULL, NULL)",
            params![
                attempt_id,
                series_id,
                comparison_id,
                request_epoch,
                ordinal + 1,
                PrReviewGuideAttemptStatus::Queued.as_str(),
                prompt_version,
                idempotency_token,
                now,
            ],
        )?;
        let attempt =
            query_pr_review_guide_attempt(&tx, &attempt_id).require("pr_review_guide_attempt", &attempt_id)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok((attempt, true))
    }

    /// Create the `work_executions` row for an attempt, following the
    /// `create_answer_agent_execution` precedent exactly: a raw insert with
    /// `work_item_id = comparison_id` (not a task), so
    /// `get_live_execution_for_work_item`-style per-comparison dedup and the
    /// generic execution machinery both work for free. Every omitted column
    /// carries its schema default.
    #[cfg(test)]
    pub(crate) fn create_pr_review_guide_execution(
        &self,
        comparison_id: &str,
        repo_remote_url: &str,
    ) -> Result<WorkExecution> {
        let conn = self.connect()?;
        insert_review_guide_execution(&conn, comparison_id, repo_remote_url)
    }

    /// Non-terminal generation attempts for this series, newest epoch first.
    /// Used to enforce "at most one active attempt per series": a duplicate
    /// observation of the same comparison is a no-op, and a newer comparison
    /// must cancel these before admitting a replacement.
    #[cfg(test)]
    pub(crate) fn live_pr_review_guide_attempts_for_series(
        &self,
        series_id: &str,
    ) -> Result<Vec<PrReviewGuideAttempt>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {PR_REVIEW_GUIDE_ATTEMPT_COLUMNS} FROM pr_review_guide_attempts
             WHERE series_id = ?1
               AND status NOT IN ('succeeded', 'failed', 'cancelled', 'superseded')
             ORDER BY request_epoch DESC, created_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map([series_id], map_pr_review_guide_attempt)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Bind an attempt to the execution generating it, mark it running, and
    /// advance the series to `generating` when this attempt is still the
    /// current desired comparison. A stale bind (series already moved on)
    /// still records the execution on the attempt but leaves card lifecycle
    /// untouched.
    #[cfg(test)]
    pub(crate) fn bind_pr_review_guide_attempt_execution(&self, attempt_id: &str, execution_id: &str) -> Result<()> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        bind_review_guide_execution(&tx, attempt_id, execution_id, &now)?;
        tx.commit()?;
        Ok(())
    }

    /// Record the resolved spawn configuration on the attempt once the
    /// engine has pinned it — diagnostics only, does not change status.
    pub(crate) fn record_pr_review_guide_attempt_spawn_config(
        &self,
        attempt_id: &str,
        driver: &str,
        model: &str,
        effort_value: &str,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE pr_review_guide_attempts SET driver = ?2, model = ?3, effort_value = ?4 WHERE id = ?1",
            params![attempt_id, driver, model, effort_value],
        )?;
        Ok(())
    }

    /// The attempt bound to `execution_id`, if any — the completion path's
    /// entry point from a driver `Stop`/terminal event back to durable state.
    pub fn pr_review_guide_attempt_for_execution(&self, execution_id: &str) -> Result<Option<PrReviewGuideAttempt>> {
        let conn = self.connect()?;
        conn.query_row(
            &format!(
                "SELECT {} FROM pr_review_guide_attempts WHERE execution_id = ?1",
                PR_REVIEW_GUIDE_ATTEMPT_COLUMNS
            ),
            [execution_id],
            map_pr_review_guide_attempt,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Transactionally fence and publish a successfully validated guide.
    ///
    /// Checks, in one transaction: the attempt is not already terminal, and
    /// its `comparison_id` still matches the series' current
    /// `selected_comparison_id` and request epoch (only the current
    /// desired comparison may advance the readable pointer). A stale attempt
    /// is recorded `superseded`, never published over newer content.
    pub fn publish_pr_review_guide_version(
        &self,
        attempt_id: &str,
        markdown: &str,
        raw_output: &str,
    ) -> Result<PublishReviewGuideOutcome> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt = query_pr_review_guide_attempt(&tx, attempt_id).require("pr_review_guide_attempt", attempt_id)?;
        if PrReviewGuideAttemptStatus::from_str(&attempt.status)?.is_terminal() {
            tx.commit()?;
            return Ok(PublishReviewGuideOutcome::AlreadyTerminal);
        }
        let (selected, epoch): (Option<String>, i64) = tx.query_row(
            "SELECT selected_comparison_id, request_epoch FROM pr_review_guide_source_series WHERE id = ?1",
            [&attempt.series_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if selected.as_deref() != Some(attempt.comparison_id.as_str()) || epoch != attempt.request_epoch {
            tx.execute(
                "UPDATE pr_review_guide_attempts SET status = ?2, finished_at = ?3 WHERE id = ?1",
                params![attempt_id, PrReviewGuideAttemptStatus::Superseded.as_str(), now],
            )?;
            tx.commit()?;
            return Ok(PublishReviewGuideOutcome::Superseded);
        }
        let content_hash = format!("{:x}", Sha256::digest(markdown.as_bytes()));
        let version_id = next_id("prgv");
        tx.execute(
            "INSERT INTO pr_review_guide_versions (
                id, series_id, comparison_id, attempt_id, markdown, raw_output, content_hash, prompt_version, generated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                version_id,
                attempt.series_id,
                attempt.comparison_id,
                attempt_id,
                markdown,
                raw_output,
                content_hash,
                attempt.prompt_version,
                now,
            ],
        )?;
        tx.execute(
            "UPDATE pr_review_guide_attempts SET status = ?2, finished_at = ?3 WHERE id = ?1",
            params![attempt_id, PrReviewGuideAttemptStatus::Succeeded.as_str(), now],
        )?;
        tx.execute(
            "UPDATE pr_review_guide_source_series
             SET guide_lifecycle = 'ready', readable_version_id = ?2, updated_at = ?3
             WHERE id = ?1",
            params![attempt.series_id, version_id, now],
        )?;
        tx.commit()?;
        Ok(PublishReviewGuideOutcome::Published(Box::new(PrReviewGuideVersion {
            id: version_id,
            series_id: attempt.series_id,
            comparison_id: attempt.comparison_id,
            attempt_id: attempt_id.to_owned(),
            markdown: markdown.to_owned(),
            raw_output: raw_output.to_owned(),
            content_hash,
            prompt_version: attempt.prompt_version,
            generated_at: now,
        })))
    }

    /// Record a classified generation failure. Never touches the series'
    /// readable pointer — a failed refresh leaves any prior readable version
    /// exactly as it was (design: "Failed refresh ... Record failure without
    /// changing readable version").
    ///
    /// A stale attempt (its comparison is no longer the series'
    /// `selected_comparison_id`, or its request epoch is obsolete) is recorded
    /// `superseded` and does **not**
    /// flip `guide_lifecycle` to `failed` — a newer published guide must
    /// not be downgraded by late output from an obsolete run.
    pub fn fail_pr_review_guide_attempt(&self, attempt_id: &str, error: &str) -> Result<()> {
        self.terminate_pr_review_guide_attempt(attempt_id, PrReviewGuideAttemptStatus::Failed, Some(error), true)
    }

    /// Record cancellation from terminal-execution reconciliation and the
    /// execution cancel/orphan hooks. Admission supersedes attempts separately.
    /// Same selected-comparison fence as [`Self::fail_pr_review_guide_attempt`]:
    /// a stale cancel leaves card lifecycle untouched.
    pub fn cancel_pr_review_guide_attempt(&self, attempt_id: &str, reason: &str) -> Result<()> {
        self.terminate_pr_review_guide_attempt(attempt_id, PrReviewGuideAttemptStatus::Cancelled, Some(reason), false)
    }

    fn terminate_pr_review_guide_attempt(
        &self,
        attempt_id: &str,
        status: PrReviewGuideAttemptStatus,
        error: Option<&str>,
        increment_retries: bool,
    ) -> Result<()> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt = query_pr_review_guide_attempt(&tx, attempt_id).require("pr_review_guide_attempt", attempt_id)?;
        if PrReviewGuideAttemptStatus::from_str(&attempt.status)?.is_terminal() {
            tx.commit()?;
            return Ok(());
        }
        let (selected, epoch): (Option<String>, i64) = tx.query_row(
            "SELECT selected_comparison_id, request_epoch FROM pr_review_guide_source_series WHERE id = ?1",
            [&attempt.series_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let stale = selected.as_deref() != Some(attempt.comparison_id.as_str()) || epoch != attempt.request_epoch;
        let final_status = if stale {
            PrReviewGuideAttemptStatus::Superseded
        } else {
            status
        };
        if increment_retries && !stale {
            tx.execute(
                "UPDATE pr_review_guide_attempts
                 SET status = ?2, error = ?3, retries = retries + 1, finished_at = ?4
                 WHERE id = ?1",
                params![attempt_id, final_status.as_str(), error, now],
            )?;
        } else {
            tx.execute(
                "UPDATE pr_review_guide_attempts
                 SET status = ?2, error = ?3, finished_at = ?4
                 WHERE id = ?1",
                params![attempt_id, final_status.as_str(), error, now],
            )?;
        }
        if !stale
            && matches!(
                final_status,
                PrReviewGuideAttemptStatus::Failed | PrReviewGuideAttemptStatus::Cancelled
            )
        {
            tx.execute(
                "UPDATE pr_review_guide_source_series
                 SET guide_lifecycle = 'failed', updated_at = ?2
                 WHERE id = ?1",
                params![attempt.series_id, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Create the `work_executions` row for a queued attempt and bind it.
    /// Idempotent: an already-bound or already-terminal attempt is returned
    /// unchanged. The root task must have a `repo_remote_url`.
    pub(crate) fn dispatch_pr_review_guide_attempt(
        &self,
        attempt_id: &str,
        root_task_id: &str,
    ) -> Result<PrReviewGuideAttempt> {
        let repo = self.repo_remote_url_for_root(root_task_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "root task {root_task_id} has no repository; cannot dispatch review-guide attempt {attempt_id}"
            )
        })?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt = query_pr_review_guide_attempt(&tx, attempt_id).require("pr_review_guide_attempt", attempt_id)?;
        if PrReviewGuideAttemptStatus::from_str(&attempt.status)?.is_terminal() || attempt.execution_id.is_some() {
            return Ok(attempt);
        }
        let existing: Option<String> = tx.query_row(
            "SELECT id FROM work_executions WHERE work_item_id = ?1 AND kind = 'pr_review_guide'
             AND status NOT IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned') ORDER BY created_at LIMIT 1",
            [&attempt.comparison_id], |row| row.get(0),
        ).optional()?;
        let execution_id = match existing {
            Some(id) => id,
            None => insert_review_guide_execution(&tx, &attempt.comparison_id, &repo)?.id,
        };
        bind_review_guide_execution(&tx, &attempt.id, &execution_id, &now_string())?;
        let bound = query_pr_review_guide_attempt(&tx, attempt_id).require("pr_review_guide_attempt", attempt_id)?;
        tx.commit()?;
        Ok(bound)
    }

    pub(crate) fn repo_remote_url_for_root(&self, root_task_id: &str) -> Result<Option<String>> {
        match self.get_work_item(root_task_id)? {
            WorkItem::Task(task) | WorkItem::Chore(task) => Ok(task.repo_remote_url),
            _ => Ok(None),
        }
    }

    /// Finish a non-terminal attempt whose bound execution has reached a
    /// terminal status. `cancelled` executions write `Cancelled`; every other
    /// terminal status writes `Failed` with `reason`. The selected-comparison
    /// fence in [`Self::terminate_pr_review_guide_attempt`] still applies.
    pub(crate) fn finish_pr_review_guide_attempt_for_terminal_execution(
        &self,
        execution_id: &str,
        execution_status: ExecutionStatus,
        reason: &str,
    ) -> Result<()> {
        let Some(attempt) = self.pr_review_guide_attempt_for_execution(execution_id)? else {
            return Ok(());
        };
        if execution_status == ExecutionStatus::Cancelled {
            self.cancel_pr_review_guide_attempt(&attempt.id, reason)
        } else {
            self.fail_pr_review_guide_attempt(&attempt.id, reason)
        }
    }

    /// Startup / sweep pass: every non-terminal attempt whose bound
    /// execution is already terminal is finished with a classified reason;
    /// every queued attempt with no execution is dispatched. Returns the
    /// number of attempts that were finished or dispatched.
    pub(crate) fn reconcile_pr_review_guide_attempts(&self) -> Result<usize> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {PR_REVIEW_GUIDE_ATTEMPT_COLUMNS} FROM pr_review_guide_attempts
             WHERE status NOT IN ('succeeded', 'failed', 'cancelled', 'superseded')"
        ))?;
        let attempts = stmt
            .query_map([], map_pr_review_guide_attempt)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);

        let mut acted = 0usize;
        for attempt in attempts {
            if let Some(execution_id) = &attempt.execution_id {
                match self.get_execution(execution_id) {
                    Ok(execution) if execution.status.is_terminal() => {
                        let reason = classified_review_guide_terminal_reason(&execution.status);
                        if let Err(err) = self.finish_pr_review_guide_attempt_for_terminal_execution(
                            execution_id,
                            execution.status,
                            &reason,
                        ) {
                            tracing::warn!(
                                attempt_id = %attempt.id,
                                execution_id,
                                ?err,
                                "review-guide reconcile: failed to finish a stranded attempt",
                            );
                        } else {
                            acted += 1;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(
                            attempt_id = %attempt.id,
                            execution_id,
                            ?err,
                            "review-guide reconcile: bound execution is gone; failing the attempt",
                        );
                        if let Err(fail_err) = self.fail_pr_review_guide_attempt(&attempt.id, "bound execution is gone")
                        {
                            tracing::warn!(
                                attempt_id = %attempt.id,
                                ?fail_err,
                                "review-guide reconcile: failed to record a vanished-execution failure",
                            );
                        } else {
                            acted += 1;
                        }
                    }
                }
            } else if attempt.status == PrReviewGuideAttemptStatus::Queued.as_str() {
                let result = self.root_task_id_for_series(&attempt.series_id).and_then(|root| {
                    let root = root.context("series has no root task")?;
                    self.dispatch_pr_review_guide_attempt(&attempt.id, &root)
                });
                match result {
                    Ok(_) => acted += 1,
                    Err(err) => {
                        let reason = format!("{err:#}");
                        let permanent = reason.contains("has no repository") || reason.contains("has no root task");
                        let retries: i64 = self.connect()?.query_row(
                            "UPDATE pr_review_guide_attempts SET retries = retries + 1, error = ?2 WHERE id = ?1 RETURNING retries",
                            params![attempt.id, reason], |row| row.get(0),
                        )?;
                        if permanent || retries >= 3 {
                            self.fail_pr_review_guide_attempt(&attempt.id, &reason)?;
                            acted += 1;
                        } else {
                            tracing::warn!(attempt_id = %attempt.id, retries, ?err, "review-guide reconcile: dispatch failed; will retry");
                        }
                    }
                }
            }
        }
        Ok(acted)
    }

    fn root_task_id_for_series(&self, series_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT root_task_id FROM pr_review_guide_source_series WHERE id = ?1",
            [series_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Series/comparison identity plus lifecycle for the `GetReviewGuide`
    /// RPC's summary half. `root_task_id` resolves the same way
    /// [`Self::get_latest_pr_review_guide_source_capture`] does.
    pub fn get_pr_review_guide_summary_for_root(&self, root_task_id: &str) -> Result<Option<PrReviewGuideSummary>> {
        let conn = self.connect()?;
        query_pr_review_guide_summary_for_root(&conn, root_task_id).map_err(Into::into)
    }

    /// One immutable version's full content, by id — the `GetReviewGuide`
    /// RPC's content half, fetched only when the caller actually wants the
    /// Markdown (board/task-detail replies use the summary alone).
    pub fn get_pr_review_guide_version(&self, version_id: &str) -> Result<Option<PrReviewGuideVersion>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, series_id, comparison_id, attempt_id, markdown, raw_output, content_hash, prompt_version, generated_at
             FROM pr_review_guide_versions WHERE id = ?1",
            [version_id],
            map_pr_review_guide_version,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Idempotently create the next attempt for a series' current desired
    /// comparison — the `RetryReviewGuide` RPC. A repeated call with the same
    /// `idempotency_token` returns the original attempt rather than creating
    /// a second one; omitting the token always creates a fresh attempt.
    ///
    /// A newly created attempt is dispatched immediately (execution row +
    /// bind). A repeated token whose original attempt was never bound is
    /// dispatched on this call so a crash between create and bind is
    /// recoverable.
    pub fn retry_pr_review_guide(
        &self,
        root_task_id: &str,
        idempotency_token: Option<&str>,
        prompt_version: &str,
    ) -> Result<RetryReviewGuideOutcome> {
        let Some(summary) = self.get_pr_review_guide_summary_for_root(root_task_id)? else {
            return Ok(RetryReviewGuideOutcome::NoComparison);
        };
        let Some(comparison_id) = summary.selected_comparison_id else {
            return Ok(RetryReviewGuideOutcome::NoComparison);
        };
        let (attempt, created) = self.admit_pr_review_guide_attempt(
            &summary.series_id,
            &comparison_id,
            prompt_version,
            idempotency_token,
            true,
        )?;
        let attempt = self.dispatch_if_unbound(attempt, root_task_id);
        Ok(if created {
            RetryReviewGuideOutcome::Created(attempt)
        } else {
            RetryReviewGuideOutcome::AlreadyRequested(attempt)
        })
    }

    fn dispatch_if_unbound(&self, attempt: PrReviewGuideAttempt, root_task_id: &str) -> PrReviewGuideAttempt {
        if attempt.execution_id.is_some()
            || PrReviewGuideAttemptStatus::from_str(&attempt.status)
                .map(|status| status.is_terminal())
                .unwrap_or(true)
        {
            return attempt;
        }
        match self.dispatch_pr_review_guide_attempt(&attempt.id, root_task_id) {
            Ok(bound) => bound,
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    root_task_id,
                    ?err,
                    "review-guide: created a durable attempt but could not dispatch its execution; reconcile will retry",
                );
                attempt
            }
        }
    }
}

fn classified_review_guide_terminal_reason(status: &ExecutionStatus) -> String {
    match status {
        ExecutionStatus::Cancelled => "execution cancelled".to_owned(),
        ExecutionStatus::Orphaned => "execution orphaned".to_owned(),
        ExecutionStatus::Abandoned => "execution abandoned".to_owned(),
        ExecutionStatus::Failed => "execution failed".to_owned(),
        ExecutionStatus::Completed => "execution completed without publishing a guide".to_owned(),
        other => format!("execution reached terminal status `{}`", other.as_str()),
    }

    /// Resolve the root task id a review-guide series belongs to. A
    /// `PrReviewGuide` execution's `work_item_id` is the comparison id, not
    /// a task id (see [`Self::review_guide_source_root_for_execution`] for
    /// the analogous execution-side resolution), so the completion
    /// finalizer needs this to get back to the owning board card from an
    /// attempt's `series_id` alone — including failure paths that never
    /// load the comparison.
    pub fn root_task_id_for_review_guide_series(&self, series_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT root_task_id FROM pr_review_guide_source_series WHERE id = ?1",
            [series_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }
}

const PR_REVIEW_GUIDE_ATTEMPT_COLUMNS: &str = "id, series_id, comparison_id, request_epoch, ordinal, execution_id, \
     status, prompt_version, driver, model, effort_value, error, retries, idempotency_token, created_at, started_at, finished_at, provider_usage_json";

fn query_pr_review_guide_attempt(conn: &Connection, attempt_id: &str) -> Result<Option<PrReviewGuideAttempt>> {
    conn.query_row(
        &format!("SELECT {PR_REVIEW_GUIDE_ATTEMPT_COLUMNS} FROM pr_review_guide_attempts WHERE id = ?1"),
        [attempt_id],
        map_pr_review_guide_attempt,
    )
    .optional()
    .map_err(Into::into)
}

fn map_pr_review_guide_attempt(row: &Row<'_>) -> rusqlite::Result<PrReviewGuideAttempt> {
    Ok(PrReviewGuideAttempt {
        id: row.get(0)?,
        series_id: row.get(1)?,
        comparison_id: row.get(2)?,
        request_epoch: row.get(3)?,
        ordinal: row.get(4)?,
        execution_id: row.get(5)?,
        status: row.get(6)?,
        prompt_version: row.get(7)?,
        driver: row.get(8)?,
        model: row.get(9)?,
        effort_value: row.get(10)?,
        error: row.get(11)?,
        retries: row.get(12)?,
        idempotency_token: row.get(13)?,
        created_at: row.get(14)?,
        started_at: row.get(15)?,
        finished_at: row.get(16)?,
        provider_usage_json: row.get(17)?,
    })
}

fn map_pr_review_guide_version(row: &Row<'_>) -> rusqlite::Result<PrReviewGuideVersion> {
    Ok(PrReviewGuideVersion {
        id: row.get(0)?,
        series_id: row.get(1)?,
        comparison_id: row.get(2)?,
        attempt_id: row.get(3)?,
        markdown: row.get(4)?,
        raw_output: row.get(5)?,
        content_hash: row.get(6)?,
        prompt_version: row.get(7)?,
        generated_at: row.get(8)?,
    })
}

fn insert_review_guide_execution(
    conn: &Connection,
    comparison_id: &str,
    repo_remote_url: &str,
) -> Result<WorkExecution> {
    let id = next_id("exec");
    let now = now_string();
    let branch_naming_json = serde_json::to_string(&boss_protocol::BranchNaming::default()).unwrap_or_default();
    conn.execute(
        "INSERT INTO work_executions (
                id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at, prefer_is_soft, pr_url, worker_branch_prefix,
                allow_dirty, branch_naming
             ) VALUES (?1, ?2, ?3, 'ready', ?4, NULL, NULL, NULL, NULL, 0, NULL, ?5, NULL, NULL, 0, NULL, NULL, 0, ?6)",
        params![
            id,
            comparison_id,
            boss_protocol::ExecutionKind::PrReviewGuide.as_str(),
            repo_remote_url,
            now,
            branch_naming_json
        ],
    )?;
    query_execution(conn, &id)?.with_context(|| format!("missing review-guide execution after insert: {id}"))
}

fn bind_review_guide_execution(tx: &Connection, attempt_id: &str, execution_id: &str, now: &str) -> Result<()> {
    let changed = tx.execute(
        "UPDATE pr_review_guide_attempts
             SET execution_id = ?2, status = ?3, started_at = ?4
             WHERE id = ?1 AND status = ?5 AND execution_id IS NULL",
        params![
            attempt_id,
            execution_id,
            PrReviewGuideAttemptStatus::Running.as_str(),
            now,
            PrReviewGuideAttemptStatus::Queued.as_str(),
        ],
    )?;
    ensure!(changed == 1, "attempt {attempt_id} is no longer queued and unbound");
    tx.execute(
        "UPDATE pr_review_guide_source_series
             SET guide_lifecycle = 'generating', updated_at = ?2
             WHERE id = (SELECT series_id FROM pr_review_guide_attempts WHERE id = ?1)
               AND request_epoch = (SELECT request_epoch FROM pr_review_guide_attempts WHERE id = ?1)
               AND selected_comparison_id = (
                   SELECT comparison_id FROM pr_review_guide_attempts WHERE id = ?1
               )",
        params![attempt_id, now],
    )?;
    Ok(())
}

/// The query body behind [`WorkDb::get_pr_review_guide_summary_for_root`],
/// factored out to a `&Connection` so a future caller already holding one
/// open (a board/task read inside a transaction) is not forced to open a
/// second one just for this lookup.
fn query_pr_review_guide_summary_for_root(
    conn: &Connection,
    root_task_id: &str,
) -> rusqlite::Result<Option<PrReviewGuideSummary>> {
    conn.query_row(
        "SELECT id, root_task_id, canonical_pr_url, guide_lifecycle, request_epoch, selected_comparison_id, readable_version_id
         FROM pr_review_guide_source_series
         WHERE root_task_id = ?1 ORDER BY latest_observation_sequence DESC, id DESC LIMIT 1",
        [root_task_id],
        |row| {
            Ok(PrReviewGuideSummary {
                series_id: row.get(0)?,
                root_task_id: row.get(1)?,
                canonical_pr_url: row.get(2)?,
                lifecycle: row.get(3)?,
                request_epoch: row.get(4)?,
                selected_comparison_id: row.get(5)?,
                readable_version_id: row.get(6)?,
            })
        },
    )
    .optional()
}

/// The `GetReviewGuideSummary` RPC's wire projection.
pub(crate) fn to_wire_review_guide_summary(summary: PrReviewGuideSummary) -> boss_protocol::ReviewGuideSummary {
    boss_protocol::ReviewGuideSummary::builder()
        .series_id(summary.series_id)
        .root_task_id(summary.root_task_id)
        .canonical_pr_url(summary.canonical_pr_url)
        .lifecycle(summary.lifecycle)
        .request_epoch(summary.request_epoch)
        .maybe_selected_comparison_id(summary.selected_comparison_id)
        .maybe_readable_version_id(summary.readable_version_id)
        .build()
}

/// Broadcast a work-item invalidation for `root_task_id` after a review-guide
/// lifecycle transition that did not itself touch the `tasks` row (guide
/// state lives in `pr_review_guide_source_series` / `_attempts` /
/// `_versions`, keyed by `root_task_id`, not in `tasks`). Mirrors the
/// `"task_doc_pointer_set"` / `"design_doc_pointer_set"` precedent in
/// `completion/pr_transition.rs`: a derived projection changed on read, so
/// subscribers (the kanban view) need a refetch hint, not a payload push.
/// Best-effort — a failure to resolve the task's product only means the
/// card catches up on its next unrelated refresh instead of immediately.
/// Takes the publisher directly (rather than `Arc<ServerState>`) so both a
/// `Dispatch` request handler (`server_state.publisher`) and
/// `WorkerCompletionHandler` (its own `publisher` field) can share it.
pub(crate) async fn notify_review_guide_changed(
    work_db: &WorkDb,
    publisher: &Arc<dyn ExecutionPublisher>,
    root_task_id: &str,
    reason: &str,
) {
    match work_db.get_work_item(root_task_id) {
        Ok(item) => {
            publisher
                .publish_work_item_changed(item.product_id(), root_task_id, reason)
                .await;
        }
        Err(err) => {
            tracing::warn!(
                root_task_id,
                reason,
                ?err,
                "notify_review_guide_changed: failed to resolve root task's product; skipping broadcast"
            );
        }
    }
}

#[cfg(test)]
#[path = "review_guide_jobs_tests.rs"]
mod tests;
