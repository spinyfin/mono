//! Durable attempts/versions for PR review-guide generation
//! (`ExecutionKind::PrReviewGuide`), and the transactional publication fence
//! that advances the series' readable pointer.
//!
//! [`super::review_guide_sources`] owns the immutable source-packet series
//! and comparison identity, including `selected_comparison_id` — the series'
//! current desired comparison. This module owns everything downstream of
//! that: one durable attempt per generation try, one immutable version per
//! successfully validated attempt, and the fence that lets only an attempt
//! bound to the CURRENT `selected_comparison_id` advance the series'
//! `readable_version_id`. A late-finishing attempt for a comparison the
//! series has since moved past is recorded as superseded history, never
//! published over newer content — design invariant #2.

use sha2::{Digest, Sha256};

use super::query_ensure::RequireRow;
use super::*;

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
    Ok(())
}

impl WorkDb {
    /// Durably record the desire to generate a guide for `comparison_id`
    /// (which must be the series' current `selected_comparison_id`) and
    /// advance the series to a new request epoch. Does not create the
    /// `work_executions` row — see [`Self::create_pr_review_guide_execution`]
    /// — mirroring the answer-agent run/execution two-phase split so a
    /// crash between the two never leaves an orphaned live execution with no
    /// durable request behind it.
    pub(crate) fn create_pr_review_guide_attempt(
        &self,
        series_id: &str,
        comparison_id: &str,
        prompt_version: &str,
    ) -> Result<PrReviewGuideAttempt> {
        self.create_pr_review_guide_attempt_with_token(series_id, comparison_id, prompt_version, None)
    }

    fn create_pr_review_guide_attempt_with_token(
        &self,
        series_id: &str,
        comparison_id: &str,
        prompt_version: &str,
        idempotency_token: Option<&str>,
    ) -> Result<PrReviewGuideAttempt> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        tx.commit()?;
        Ok(attempt)
    }

    /// Create the `work_executions` row for an attempt, following the
    /// `create_answer_agent_execution` precedent exactly: a raw insert with
    /// `work_item_id = comparison_id` (not a task), so
    /// `get_live_execution_for_work_item`-style per-comparison dedup and the
    /// generic execution machinery both work for free. Every omitted column
    /// carries its schema default.
    pub(crate) fn create_pr_review_guide_execution(
        &self,
        comparison_id: &str,
        repo_remote_url: &str,
    ) -> Result<WorkExecution> {
        let conn = self.connect()?;
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
        query_execution(&conn, &id)?.with_context(|| format!("missing review-guide execution after insert: {id}"))
    }

    /// Non-terminal generation attempts for this series, newest epoch first.
    /// Used to enforce "at most one active attempt per series": a duplicate
    /// observation of the same comparison is a no-op, and a newer comparison
    /// must cancel these before admitting a replacement.
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
    pub(crate) fn bind_pr_review_guide_attempt_execution(&self, attempt_id: &str, execution_id: &str) -> Result<()> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE pr_review_guide_attempts
             SET execution_id = ?2, status = ?3, started_at = ?4
             WHERE id = ?1 AND status = ?5",
            params![
                attempt_id,
                execution_id,
                PrReviewGuideAttemptStatus::Running.as_str(),
                now,
                PrReviewGuideAttemptStatus::Queued.as_str(),
            ],
        )?;
        tx.execute(
            "UPDATE pr_review_guide_source_series
             SET guide_lifecycle = 'generating', updated_at = ?2
             WHERE id = (SELECT series_id FROM pr_review_guide_attempts WHERE id = ?1)
               AND selected_comparison_id = (
                   SELECT comparison_id FROM pr_review_guide_attempts WHERE id = ?1
               )",
            params![attempt_id, now],
        )?;
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
    /// `selected_comparison_id` (design invariant #2 — only the current
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
        let selected: Option<String> = tx.query_row(
            "SELECT selected_comparison_id FROM pr_review_guide_source_series WHERE id = ?1",
            [&attempt.series_id],
            |row| row.get(0),
        )?;
        if selected.as_deref() != Some(attempt.comparison_id.as_str()) {
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
    /// `selected_comparison_id`) is recorded `superseded` and does **not**
    /// flip `guide_lifecycle` to `failed` — a newer published guide must
    /// not be downgraded by late output from an obsolete run.
    pub fn fail_pr_review_guide_attempt(&self, attempt_id: &str, error: &str) -> Result<()> {
        self.terminate_pr_review_guide_attempt(attempt_id, PrReviewGuideAttemptStatus::Failed, Some(error), true)
    }

    /// Record that generation was cancelled (operator cancel, a newer
    /// comparison admitted, or an explicit retry replacing this attempt).
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
        let selected: Option<String> = tx.query_row(
            "SELECT selected_comparison_id FROM pr_review_guide_source_series WHERE id = ?1",
            [&attempt.series_id],
            |row| row.get(0),
        )?;
        let stale = selected.as_deref() != Some(attempt.comparison_id.as_str());
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
        let conn = self.connect()?;
        let attempt =
            query_pr_review_guide_attempt(&conn, attempt_id).require("pr_review_guide_attempt", attempt_id)?;
        drop(conn);
        if PrReviewGuideAttemptStatus::from_str(&attempt.status)?.is_terminal() || attempt.execution_id.is_some() {
            return Ok(attempt);
        }
        let repo = self.repo_remote_url_for_root(root_task_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "root task {root_task_id} has no repository; cannot dispatch review-guide attempt {attempt_id}"
            )
        })?;
        let execution = self.create_pr_review_guide_execution(&attempt.comparison_id, &repo)?;
        self.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)?;
        let conn = self.connect()?;
        query_pr_review_guide_attempt(&conn, attempt_id).require("pr_review_guide_attempt", attempt_id)
    }

    fn repo_remote_url_for_root(&self, root_task_id: &str) -> Result<Option<String>> {
        match self.get_work_item(root_task_id)? {
            WorkItem::Task(task) | WorkItem::Chore(task) => Ok(task.repo_remote_url),
            _ => Ok(None),
        }
    }

    /// Cancel every non-terminal attempt for `series_id` (and its bound
    /// execution, if any). Used before admitting a newer comparison or an
    /// explicit retry so two Astra-high jobs cannot run for one PR at once.
    pub(crate) fn cancel_live_pr_review_guide_attempts_for_series(
        &self,
        series_id: &str,
        reason: &str,
    ) -> Result<Vec<PrReviewGuideAttempt>> {
        let live = self.live_pr_review_guide_attempts_for_series(series_id)?;
        for attempt in &live {
            if let Err(err) = self.cancel_pr_review_guide_attempt(&attempt.id, reason) {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    series_id,
                    ?err,
                    "review-guide: failed to cancel an obsolete attempt",
                );
            }
            if let Some(execution_id) = &attempt.execution_id
                && let Err(err) = self.cancel_execution(execution_id)
            {
                tracing::debug!(
                    attempt_id = %attempt.id,
                    execution_id,
                    ?err,
                    "review-guide: obsolete execution already terminal or unknown",
                );
            }
        }
        Ok(live)
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
                match self.root_task_id_for_series(&attempt.series_id) {
                    Ok(Some(root)) => match self.dispatch_pr_review_guide_attempt(&attempt.id, &root) {
                        Ok(_) => acted += 1,
                        Err(err) => tracing::warn!(
                            attempt_id = %attempt.id,
                            ?err,
                            "review-guide reconcile: could not dispatch a queued unbound attempt",
                        ),
                    },
                    Ok(None) => tracing::warn!(
                        attempt_id = %attempt.id,
                        series_id = %attempt.series_id,
                        "review-guide reconcile: series has no root task; leaving the attempt queued",
                    ),
                    Err(err) => tracing::warn!(
                        attempt_id = %attempt.id,
                        series_id = %attempt.series_id,
                        ?err,
                        "review-guide reconcile: failed to resolve the series root task",
                    ),
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
        conn.query_row(
            "SELECT id, root_task_id, canonical_pr_url, guide_lifecycle, request_epoch, selected_comparison_id, readable_version_id
             FROM pr_review_guide_source_series
             WHERE root_task_id = ?1 ORDER BY updated_at DESC, id DESC LIMIT 1",
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
        .map_err(Into::into)
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
        if let Some(token) = idempotency_token {
            let conn = self.connect()?;
            let existing = conn
                .query_row(
                    &format!(
                        "SELECT {} FROM pr_review_guide_attempts WHERE series_id = ?1 AND idempotency_token = ?2",
                        PR_REVIEW_GUIDE_ATTEMPT_COLUMNS
                    ),
                    params![summary.series_id, token],
                    map_pr_review_guide_attempt,
                )
                .optional()?;
            if let Some(existing) = existing {
                let existing = self.dispatch_if_unbound(existing, root_task_id);
                return Ok(RetryReviewGuideOutcome::AlreadyRequested(existing));
            }
        }
        let _ =
            self.cancel_live_pr_review_guide_attempts_for_series(&summary.series_id, "replaced by an explicit retry");
        let attempt = self.create_pr_review_guide_attempt_with_token(
            &summary.series_id,
            &comparison_id,
            prompt_version,
            idempotency_token,
        )?;
        let attempt = self.dispatch_if_unbound(attempt, root_task_id);
        Ok(RetryReviewGuideOutcome::Created(attempt))
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
}

const PR_REVIEW_GUIDE_ATTEMPT_COLUMNS: &str = "id, series_id, comparison_id, request_epoch, ordinal, execution_id, \
     status, prompt_version, driver, model, effort_value, error, retries, idempotency_token, created_at, started_at, finished_at";

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_active_chore, create_product, open_db};
    use boss_pr_review_sources::SourcePacket;

    fn packet(base: &str, head: &str) -> SourcePacket {
        SourcePacket {
            schema_version: 2,
            canonical_pr_url: "https://github.com/acme/widget/pull/9".to_owned(),
            pr_number: 9,
            title: "Fix retry".to_owned(),
            body: None,
            base_repository: "acme/widget".to_owned(),
            head_repository: "acme/widget".to_owned(),
            observed_base_sha: base.to_owned(),
            probe_base_sha: None,
            merge_base_sha: base.to_owned(),
            head_sha: head.to_owned(),
            files: Vec::new(),
            omissions: Vec::new(),
        }
    }

    fn seeded_series(db: &WorkDb) -> (String, String, String) {
        let product = create_product(db);
        let root = create_active_chore(db, &product, "review guide job test");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
                params!["https://github.com/acme/widget.git", root],
            )
            .unwrap();
        let stored = db
            .persist_pr_review_guide_source_capture(&root, 1, PrSourceCaptureTrigger::Creation, &packet("base", "head"))
            .unwrap();
        let PrSourceCapturePersistOutcome::Stored(capture) = stored else {
            panic!("capture must persist")
        };
        (root, capture.series_id, capture.comparison_id)
    }

    #[test]
    fn attempt_then_execution_then_publish_advances_the_readable_pointer() {
        let (_dir, db) = open_db();
        let (root, series_id, comparison_id) = seeded_series(&db);
        let attempt = db
            .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
            .unwrap();
        assert_eq!(attempt.status, "queued");
        assert_eq!(attempt.request_epoch, 1);

        let execution = db
            .create_pr_review_guide_execution(&comparison_id, "acme/widget")
            .unwrap();
        assert_eq!(execution.work_item_id, comparison_id);
        assert_eq!(execution.kind, boss_protocol::ExecutionKind::PrReviewGuide);
        db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
            .unwrap();

        let bound = db
            .pr_review_guide_attempt_for_execution(&execution.id)
            .unwrap()
            .unwrap();
        assert_eq!(bound.status, "running");
        let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
        assert_eq!(summary.lifecycle, "generating");

        let outcome = db
            .publish_pr_review_guide_version(&attempt.id, "# Guide\n\n## Problem\n", "raw")
            .unwrap();
        let PublishReviewGuideOutcome::Published(version) = outcome else {
            panic!("must publish")
        };
        assert_eq!(version.markdown, "# Guide\n\n## Problem\n");

        let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
        assert_eq!(summary.lifecycle, "ready");
        assert_eq!(summary.readable_version_id.as_deref(), Some(version.id.as_str()));

        let fetched = db.get_pr_review_guide_version(&version.id).unwrap().unwrap();
        assert_eq!(fetched.markdown, version.markdown);
    }

    #[test]
    fn publish_for_a_superseded_comparison_does_not_advance_the_pointer() {
        let (_dir, db) = open_db();
        let (root, series_id, first_comparison) = seeded_series(&db);
        let first_attempt = db
            .create_pr_review_guide_attempt(&series_id, &first_comparison, "review-guide-v1")
            .unwrap();

        // A newer comparison is captured and selected while the first attempt
        // is still in flight.
        db.persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &packet("base2", "head2"))
            .unwrap();

        let outcome = db
            .publish_pr_review_guide_version(&first_attempt.id, "# Stale guide\n\n## X\n## Y\n## Z\n## W\n", "raw")
            .unwrap();
        assert_eq!(outcome, PublishReviewGuideOutcome::Superseded);

        let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
        assert_eq!(
            summary.lifecycle, "queued",
            "stale publish must not flip lifecycle to ready"
        );
        assert!(summary.readable_version_id.is_none());
    }

    #[test]
    fn publish_after_terminal_is_a_noop() {
        let (_dir, db) = open_db();
        let (_root, series_id, comparison_id) = seeded_series(&db);
        let attempt = db
            .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
            .unwrap();
        db.fail_pr_review_guide_attempt(&attempt.id, "boom").unwrap();
        let outcome = db
            .publish_pr_review_guide_version(&attempt.id, "# Late\n\n## A\n## B\n## C\n## D\n", "raw")
            .unwrap();
        assert_eq!(outcome, PublishReviewGuideOutcome::AlreadyTerminal);
    }

    #[test]
    fn failure_records_error_without_touching_a_prior_readable_version() {
        let (_dir, db) = open_db();
        let (root, series_id, comparison_id) = seeded_series(&db);
        let first = db
            .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
            .unwrap();
        db.publish_pr_review_guide_version(&first.id, "# Good\n\n## A\n## B\n## C\n## D\n", "raw")
            .unwrap();
        let readable_before = db
            .get_pr_review_guide_summary_for_root(&root)
            .unwrap()
            .unwrap()
            .readable_version_id;

        // Explicit regenerate against the SAME comparison; this attempt fails.
        let retry = db.retry_pr_review_guide(&root, None, "review-guide-v1").unwrap();
        let RetryReviewGuideOutcome::Created(retry_attempt) = retry else {
            panic!("retry must create a new attempt")
        };
        db.fail_pr_review_guide_attempt(&retry_attempt.id, "model unavailable")
            .unwrap();

        let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
        assert_eq!(summary.lifecycle, "failed");
        assert_eq!(
            summary.readable_version_id, readable_before,
            "old readable version must survive a failed refresh"
        );
    }

    #[test]
    fn retry_with_the_same_idempotency_token_returns_the_original_attempt() {
        let (_dir, db) = open_db();
        let (root, ..) = seeded_series(&db);
        let first = db
            .retry_pr_review_guide(&root, Some("tok-1"), "review-guide-v1")
            .unwrap();
        let RetryReviewGuideOutcome::Created(first_attempt) = first else {
            panic!("first call must create")
        };
        let second = db
            .retry_pr_review_guide(&root, Some("tok-1"), "review-guide-v1")
            .unwrap();
        let RetryReviewGuideOutcome::AlreadyRequested(second_attempt) = second else {
            panic!("repeat token must not create a second attempt")
        };
        assert_eq!(first_attempt.id, second_attempt.id);
        assert!(
            first_attempt.execution_id.is_some(),
            "retry must create and bind a work_executions row, not leave the attempt queued forever"
        );
        assert_eq!(first_attempt.execution_id, second_attempt.execution_id);
        assert_eq!(first_attempt.status, "running");
    }

    #[test]
    fn retry_without_any_captured_comparison_reports_no_comparison() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "no capture yet");
        assert_eq!(
            db.retry_pr_review_guide(&root, None, "review-guide-v1").unwrap(),
            RetryReviewGuideOutcome::NoComparison
        );
    }

    fn attempt_status(db: &WorkDb, attempt_id: &str) -> String {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT status FROM pr_review_guide_attempts WHERE id = ?1",
                [attempt_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn failing_a_stale_attempt_does_not_downgrade_a_newer_ready_series() {
        let (_dir, db) = open_db();
        let (root, series_id, first_comparison) = seeded_series(&db);
        let first = db
            .create_pr_review_guide_attempt(&series_id, &first_comparison, "review-guide-v1")
            .unwrap();

        db.persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &packet("base2", "head2"))
            .unwrap();
        let second_comparison = db
            .get_pr_review_guide_summary_for_root(&root)
            .unwrap()
            .unwrap()
            .selected_comparison_id
            .unwrap();
        let second = db
            .create_pr_review_guide_attempt(&series_id, &second_comparison, "review-guide-v1")
            .unwrap();
        db.publish_pr_review_guide_version(&second.id, "# Current\n\n## A\n## B\n## C\n## D\n", "raw")
            .unwrap();

        db.fail_pr_review_guide_attempt(&first.id, "late invalid output")
            .unwrap();

        assert_eq!(attempt_status(&db, &first.id), "superseded");
        let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
        assert_eq!(summary.lifecycle, "ready");
    }

    #[test]
    fn reconcile_finishes_a_running_attempt_whose_execution_is_already_terminal() {
        let (_dir, db) = open_db();
        let (root, series_id, comparison_id) = seeded_series(&db);
        let attempt = db
            .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
            .unwrap();
        let execution = db
            .create_pr_review_guide_execution(&comparison_id, "https://github.com/acme/widget.git")
            .unwrap();
        db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
            .unwrap();
        // Terminalize the execution row without going through cancel_execution,
        // which is the stranded shape the sweep has to recover: orphan reap,
        // abandon, or a restart that lost the run.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'orphaned', finished_at = datetime('now') WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();

        let acted = db.reconcile_pr_review_guide_attempts().unwrap();
        assert_eq!(acted, 1);
        assert_eq!(attempt_status(&db, &attempt.id), "failed");
        let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
        assert_eq!(summary.lifecycle, "failed");
    }

    #[test]
    fn cancel_live_attempts_enforces_one_active_attempt_per_series() {
        let (_dir, db) = open_db();
        let (root, series_id, _first_comparison) = seeded_series(&db);
        let first = db.retry_pr_review_guide(&root, None, "review-guide-v1").unwrap();
        let RetryReviewGuideOutcome::Created(first) = first else {
            panic!("must create")
        };
        assert_eq!(first.status, "running");

        db.persist_pr_review_guide_source_capture(&root, 2, PrSourceCaptureTrigger::Poller, &packet("base2", "head2"))
            .unwrap();
        let cancelled = db
            .cancel_live_pr_review_guide_attempts_for_series(&series_id, "superseded by a newer comparison")
            .unwrap();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].id, first.id);

        let status: String = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT status FROM pr_review_guide_attempts WHERE id = ?1",
                [&first.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "superseded",
            "the first comparison is no longer selected, so cancel records superseded not failed"
        );
        let exec = db.get_execution(first.execution_id.as_deref().unwrap()).unwrap();
        assert_eq!(exec.status, ExecutionStatus::Cancelled);
    }

    #[test]
    fn cancel_execution_writes_cancelled_on_the_bound_attempt() {
        let (_dir, db) = open_db();
        let (root, ..) = seeded_series(&db);
        let RetryReviewGuideOutcome::Created(attempt) =
            db.retry_pr_review_guide(&root, None, "review-guide-v1").unwrap()
        else {
            panic!("must create")
        };
        db.cancel_execution(attempt.execution_id.as_deref().unwrap()).unwrap();
        assert_eq!(attempt_status(&db, &attempt.id), "cancelled");
        let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
        assert_eq!(summary.lifecycle, "failed");
    }

    #[test]
    fn reconcile_dispatches_a_queued_attempt_with_no_execution() {
        let (_dir, db) = open_db();
        let (root, series_id, comparison_id) = seeded_series(&db);
        let attempt = db
            .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
            .unwrap();
        assert!(attempt.execution_id.is_none());

        let acted = db.reconcile_pr_review_guide_attempts().unwrap();
        assert_eq!(acted, 1);
        let bound = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT execution_id, status FROM pr_review_guide_attempts WHERE id = ?1",
                [&attempt.id],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap();
        assert!(bound.0.is_some());
        assert_eq!(bound.1, "running");
        let _ = root;
    }
}
