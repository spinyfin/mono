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
    /// Lossless provider usage snapshots collected through the execution hook path.
    pub provider_usage_json: Option<String>,
    /// The attempt failed before a worker session started (spawn refused,
    /// workspace check failed, pane never came up), as opposed to failing
    /// after a worker ran. Only these are eligible for
    /// [`WorkDb::reenqueue_pre_start_failed_review_guides`].
    #[builder(default)]
    pub failed_pre_start: bool,
    /// Identity of the engine build that recorded the failure (the one
    /// configured on this database handle). `None` for
    /// attempts that did not fail, or failed before this column existed or
    /// with no identity installed (their build is unknowable, so they are
    /// never retried).
    pub failed_by_build: Option<String>,
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
    pub error: Option<String>,
}

/// One guide re-enqueued by [`WorkDb::reenqueue_pre_start_failed_review_guides`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReenqueuedReviewGuide {
    pub root_task_id: String,
    pub pr_url: String,
    /// The pre-start-failed attempt left in place as history.
    pub failed_attempt_id: String,
    pub new_attempt_id: String,
}

/// What one startup re-enqueue pass did, for the log line, the audit record
/// and the engine health report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewGuideReenqueueReport {
    /// The running build the pass compared failed attempts against.
    pub build: String,
    pub reenqueued: Vec<ReenqueuedReviewGuide>,
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

/// What a failure records beyond the error text: whether it happened before
/// a worker started, and which engine build observed it (`None` when the
/// build is unknowable, which keeps the attempt out of build-change retries).
#[derive(Clone, Copy)]
struct FailureProvenance<'a> {
    pre_start: bool,
    build: Option<&'a str>,
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Read an attempt, including its durable failure and recovery state.
    pub fn get_pr_review_guide_attempt(&self, attempt_id: &str) -> Result<Option<PrReviewGuideAttempt>> {
        let conn = self.connect()?;
        query_pr_review_guide_attempt(&conn, attempt_id)
    }

    /// Admit a request for the selected comparison, reusing a live request
    /// for that comparison or atomically replacing an obsolete request.
    /// The durable queued attempt can be dispatched by enqueue or reconcile.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn create_pr_review_guide_attempt(
        &self,
        series_id: &str,
        comparison_id: &str,
        prompt_version: &str,
    ) -> Result<PrReviewGuideAttempt> {
        self.admit_pr_review_guide_attempt(series_id, comparison_id, prompt_version, None, false, false)
            .map(|(attempt, _)| attempt)
    }

    pub(crate) fn create_automatic_pr_review_guide_attempt(
        &self,
        series_id: &str,
        comparison_id: &str,
    ) -> Result<PrReviewGuideAttempt> {
        self.admit_pr_review_guide_attempt(
            series_id,
            comparison_id,
            boss_review_guide::PROMPT_VERSION,
            None,
            false,
            true,
        )
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
        self.admit_pr_review_guide_attempt(series_id, comparison_id, prompt_version, idempotency_token, true, false)
            .map(|(attempt, _)| attempt)
    }

    fn admit_pr_review_guide_attempt(
        &self,
        series_id: &str,
        comparison_id: &str,
        prompt_version: &str,
        idempotency_token: Option<&str>,
        replace: bool,
        incremental: bool,
    ) -> Result<(PrReviewGuideAttempt, bool)> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pending = PendingEvents::new();
        if let Some(token) = idempotency_token {
            let existing = tx.query_row(
                &format!("SELECT {PR_REVIEW_GUIDE_ATTEMPT_COLUMNS} FROM pr_review_guide_attempts WHERE series_id = ?1 AND (idempotency_token = ?2 OR id IN (SELECT attempt_id FROM pr_review_guide_request_tokens WHERE series_id = ?1 AND token = ?2))"),
                params![series_id, token], map_pr_review_guide_attempt,
            ).optional()?;
            if let Some(existing) = existing {
                return Ok((existing, false));
            }
        }
        let (selected, epoch): (Option<String>, i64) = tx.query_row(
            "SELECT selected_comparison_id, request_epoch FROM pr_review_guide_source_series WHERE id = ?1",
            [series_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
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
        if !replace
            && let Some(existing) = live
                .iter()
                .find(|a| a.comparison_id == comparison_id && a.request_epoch == epoch)
        {
            // Remember every coalesced click's token, so replay after this
            // attempt finishes still returns it instead of spending again.
            if let Some(token) = idempotency_token {
                tx.execute(
                    "INSERT INTO pr_review_guide_request_tokens (series_id, token, attempt_id) VALUES (?1, ?2, ?3)",
                    params![series_id, token, existing.id],
                )?;
            }
            tx.commit()?;
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
        // Only the automatic, revision-triggered path may start from the
        // published version; an explicit Generate/Regenerate is a full run.
        if incremental {
            tx.execute(
                "INSERT INTO pr_review_guide_automatic_attempts (attempt_id) VALUES (?1)",
                [&attempt_id],
            )?;
            guide_updates::snapshot(&tx, &attempt_id, series_id, comparison_id)?;
        }
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
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn create_pr_review_guide_execution(
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
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn bind_pr_review_guide_attempt_execution(&self, attempt_id: &str, execution_id: &str) -> Result<()> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        bind_review_guide_execution(&tx, attempt_id, execution_id, &now)?;
        tx.commit()?;
        Ok(())
    }

    /// Record the resolved spawn configuration on the attempt once the
    /// engine has pinned it, before spawning. Recovery uses this configuration
    /// if the execution launch stamp was not written; does not change status.
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
        let outcome = publish_pr_review_guide_version_in_tx(&tx, attempt_id, markdown, raw_output, &now)?;
        tx.commit()?;
        Ok(outcome)
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
    ///
    /// Records the build configured on this database handle.
    pub fn fail_pr_review_guide_attempt(&self, attempt_id: &str, error: &str) -> Result<()> {
        self.fail_pr_review_guide_attempt_for_build(attempt_id, error, false, self.engine_build_identity.as_deref())
    }

    /// [`Self::fail_pr_review_guide_attempt`] for a failure that happened
    /// before any worker session started. Records that fact (and the failing
    /// build) so [`Self::reenqueue_pre_start_failed_review_guides`] can retry
    /// the attempt once a different build is running.
    pub fn fail_pr_review_guide_attempt_pre_start(&self, attempt_id: &str, error: &str) -> Result<()> {
        self.fail_pr_review_guide_attempt_for_build(attempt_id, error, true, self.engine_build_identity.as_deref())
    }

    /// [`Self::fail_pr_review_guide_attempt`] with the failing build named by
    /// the caller instead of the injected identity.
    pub(crate) fn fail_pr_review_guide_attempt_at(
        &self,
        attempt_id: &str,
        error: &str,
        pre_start: bool,
        build: &str,
    ) -> Result<()> {
        self.fail_pr_review_guide_attempt_for_build(attempt_id, error, pre_start, Some(build))
    }

    fn fail_pr_review_guide_attempt_for_build(
        &self,
        attempt_id: &str,
        error: &str,
        pre_start: bool,
        build: Option<&str>,
    ) -> Result<()> {
        self.terminate_pr_review_guide_attempt(
            attempt_id,
            PrReviewGuideAttemptStatus::Failed,
            Some(error),
            Some(FailureProvenance { pre_start, build }),
        )
    }

    /// Record cancellation from terminal-execution reconciliation and the
    /// execution cancel/orphan hooks. Admission supersedes attempts separately.
    /// Same selected-comparison fence as [`Self::fail_pr_review_guide_attempt`]:
    /// a stale cancel leaves card lifecycle untouched.
    pub fn cancel_pr_review_guide_attempt(&self, attempt_id: &str, reason: &str) -> Result<()> {
        self.terminate_pr_review_guide_attempt(attempt_id, PrReviewGuideAttemptStatus::Cancelled, Some(reason), None)
    }

    fn terminate_pr_review_guide_attempt(
        &self,
        attempt_id: &str,
        status: PrReviewGuideAttemptStatus,
        error: Option<&str>,
        failure: Option<FailureProvenance<'_>>,
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
        if let Some(failure) = failure
            && !stale
        {
            tx.execute(
                "UPDATE pr_review_guide_attempts
                 SET status = ?2, error = ?3, retries = retries + 1, finished_at = ?4,
                     failed_pre_start = ?5, failed_by_build = ?6
                 WHERE id = ?1",
                params![
                    attempt_id,
                    final_status.as_str(),
                    error,
                    now,
                    failure.pre_start,
                    failure.build
                ],
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
    /// unchanged. The root task must resolve to a repository (override or product).
    pub(crate) fn dispatch_pr_review_guide_attempt(
        &self,
        attempt_id: &str,
        root_task_id: &str,
    ) -> Result<PrReviewGuideAttempt> {
        let repo = self.resolve_repo_for_task(root_task_id)?.ok_or_else(|| {
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
        if guide_updates::chain_has_active_writer(&tx, &attempt.id)? {
            // Hold the incremental update until the revision chain has no
            // writer; the attempt stays queued and a later dispatch pass binds it.
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

    /// Finish a non-terminal attempt whose bound execution has reached a
    /// terminal status. `cancelled` executions write `Cancelled`; every other
    /// terminal status writes `Failed` with `reason`. The selected-comparison
    /// fence in [`Self::terminate_pr_review_guide_attempt`] still applies.
    ///
    /// `pre_start` marks a failure that happened before any worker session
    /// started; see [`Self::fail_pr_review_guide_attempt_pre_start`]. Callers
    /// that cannot tell pass `false`: an unrecognised failure is never retried
    /// automatically on a build change.
    pub(crate) fn finish_pr_review_guide_attempt_for_terminal_execution(
        &self,
        execution_id: &str,
        execution_status: ExecutionStatus,
        reason: &str,
        pre_start: bool,
    ) -> Result<()> {
        let Some(attempt) = self.pr_review_guide_attempt_for_execution(execution_id)? else {
            return Ok(());
        };
        if execution_status == ExecutionStatus::Cancelled {
            self.cancel_pr_review_guide_attempt(&attempt.id, reason)
        } else if pre_start {
            self.fail_pr_review_guide_attempt_pre_start(&attempt.id, reason)
        } else {
            self.fail_pr_review_guide_attempt(&attempt.id, reason)
        }
    }

    /// Startup / sweep pass over every non-terminal attempt.
    ///
    /// - An attempt bound to an execution that has reached a terminal status
    ///   is finished with a classified reason — **unless** that status is
    ///   `orphaned` and the worker is still live. `orphaned` is a death the
    ///   engine *inferred* from missing signal, never a decision (see
    ///   [`crate::worker_readoption::classify_contradiction`]); an orphaned
    ///   row with a live process is exactly the contradiction re-adoption
    ///   resolves by putting the execution back to `running`, and an attempt
    ///   closed in the meantime would leave that re-adopted run with nothing
    ///   to submit its guide to (a guide run still working after an engine
    ///   restart would otherwise have no running attempt to submit to). "Live" is the
    ///   injected bounded process observation or the caller's own
    ///   evidence in `known_live_execution_ids` — startup passes the
    ///   executions the tmux adoption pass just observed alive. A genuinely
    ///   stranded attempt (no live process) is still finished.
    /// - An attempt whose bound execution row no longer exists is failed. A
    ///   *read error* is not that: the attempt is left for the next pass
    ///   rather than failed on a transient fault.
    /// - A queued attempt with no execution is dispatched.
    ///
    /// Every action is logged with the attempt id, execution id and reason.
    /// Returns the number of attempts finished or dispatched.
    pub fn reconcile_pr_review_guide_attempts_with_live_runs(
        &self,
        known_live_execution_ids: &HashSet<String>,
        execution_is_alive: &dyn Fn(&str) -> bool,
    ) -> Result<usize> {
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
            let Some(execution_id) = attempt.execution_id.clone() else {
                if attempt.status == PrReviewGuideAttemptStatus::Queued.as_str() {
                    acted += usize::from(self.reconcile_dispatch_unbound_attempt(&attempt)?);
                }
                continue;
            };
            let execution = {
                let conn = self.connect()?;
                query_execution(&conn, &execution_id)
            };
            let execution = match execution {
                Ok(Some(execution)) => execution,
                Ok(None) => {
                    let reason = "bound execution is gone";
                    match self.fail_pr_review_guide_attempt(&attempt.id, reason) {
                        Ok(()) => {
                            tracing::warn!(
                                attempt_id = %attempt.id,
                                execution_id = %execution_id,
                                reason,
                                action = "failed",
                                "review-guide reconcile: bound execution row no longer exists; failed the attempt",
                            );
                            acted += 1;
                        }
                        Err(fail_err) => tracing::warn!(
                            attempt_id = %attempt.id,
                            execution_id = %execution_id,
                            ?fail_err,
                            "review-guide reconcile: failed to record a vanished-execution failure",
                        ),
                    }
                    continue;
                }
                Err(err) => {
                    // A failed read says nothing about the execution; failing
                    // the attempt on it would be the same fault as closing a
                    // live run. Leave it for the next pass.
                    tracing::warn!(
                        attempt_id = %attempt.id,
                        execution_id = %execution_id,
                        ?err,
                        "review-guide reconcile: could not read the bound execution; leaving the attempt untouched",
                    );
                    continue;
                }
            };
            if !execution.status.is_terminal() {
                continue;
            }
            if execution.status == ExecutionStatus::Orphaned {
                let liveness = if known_live_execution_ids.contains(&execution_id) {
                    Some("observed_live_by_caller")
                } else {
                    execution_is_alive(&execution_id).then_some("observed_live_by_process_observer")
                };
                if let Some(evidence) = liveness {
                    tracing::info!(
                        attempt_id = %attempt.id,
                        execution_id = %execution_id,
                        evidence,
                        action = "kept_open",
                        "review-guide reconcile: execution is orphaned but its worker is live; keeping the \
                         attempt running for re-adoption",
                    );
                    continue;
                }
            }
            let reason = execution
                .last_error
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| classified_review_guide_terminal_reason(&execution.status));
            match self.finish_pr_review_guide_attempt_for_terminal_execution(
                &execution_id,
                execution.status.clone(),
                &reason,
                false,
            ) {
                Ok(()) => {
                    tracing::warn!(
                        attempt_id = %attempt.id,
                        execution_id = %execution_id,
                        execution_status = %execution.status,
                        reason = %reason,
                        action = "finished",
                        "review-guide reconcile: bound execution is terminal with no live worker; finished the attempt",
                    );
                    acted += 1;
                }
                Err(err) => tracing::warn!(
                    attempt_id = %attempt.id,
                    execution_id = %execution_id,
                    ?err,
                    "review-guide reconcile: failed to finish a stranded attempt",
                ),
            }
        }
        Ok(acted)
    }

    /// Dispatch a queued attempt that has no execution. Returns whether the
    /// pass acted on it (dispatched, or failed after exhausting retries).
    fn reconcile_dispatch_unbound_attempt(&self, attempt: &PrReviewGuideAttempt) -> Result<bool> {
        let result = self.root_task_id_for_series(&attempt.series_id).and_then(|root| {
            let root = root.context("series has no root task")?;
            self.dispatch_pr_review_guide_attempt(&attempt.id, &root)
        });
        match result {
            Ok(bound) => {
                tracing::info!(
                    attempt_id = %attempt.id,
                    execution_id = ?bound.execution_id,
                    action = "dispatched",
                    "review-guide reconcile: dispatched a queued attempt with no execution",
                );
                Ok(true)
            }
            Err(err) => {
                let reason = format!("{err:#}");
                let permanent = reason.contains("has no repository") || reason.contains("has no root task");
                let retries: i64 = self.connect()?.query_row(
                    "UPDATE pr_review_guide_attempts SET retries = retries + 1, error = ?2 WHERE id = ?1 RETURNING retries",
                    params![attempt.id, reason],
                    |row| row.get(0),
                )?;
                if permanent || retries >= 3 {
                    self.fail_pr_review_guide_attempt(&attempt.id, &reason)?;
                    tracing::warn!(
                        attempt_id = %attempt.id,
                        retries,
                        reason = %reason,
                        action = "failed",
                        "review-guide reconcile: dispatch failed permanently; failed the attempt",
                    );
                    Ok(true)
                } else {
                    tracing::warn!(
                        attempt_id = %attempt.id,
                        retries,
                        ?err,
                        "review-guide reconcile: dispatch failed; will retry"
                    );
                    Ok(false)
                }
            }
        }
    }

    /// Startup pass: give every review guide that failed *before a worker
    /// started* one more try, now that a different engine build is running.
    ///
    /// A pre-start failure (spawn refused, workspace check failed, pane never
    /// came up) says nothing about the PR and often nothing about the
    /// prompt: it is a defect in the build that tried to spawn. Reconcile
    /// leaves such an attempt terminal `failed`, so without this pass an
    /// outage (incident 008) strands every guide it touched until someone
    /// retries each PR by hand.
    ///
    /// An attempt is re-enqueued only when all of these hold:
    /// - it failed before start, on a build other than `running_build`;
    /// - its comparison is still the series' selected one, no newer attempt
    ///   exists in the series, and no guide was published for that head;
    /// - the root task is live and still on this series' PR, and a live PR
    ///   probe confirms it is open (probe failures are skipped).
    ///
    /// Re-enqueueing goes through [`Self::request_pr_review_guide`], the same
    /// path as a manual retry: it creates a queued attempt and a `ready`
    /// execution, which the coordinator dispatches through the review pool
    /// and the dispatch pause like any other guide. Nothing is dispatched
    /// here, and old attempts are never edited or deleted.
    ///
    /// The new attempt records `running_build` if it too fails before start,
    /// so a restart on the same build does not retry it again; a later,
    /// different build gets one more try.
    pub(crate) fn reenqueue_pre_start_failed_review_guides(
        &self,
        running_build: &str,
        pr_checker: &dyn PrStateChecker,
    ) -> Result<ReviewGuideReenqueueReport> {
        struct Candidate {
            attempt_id: String,
            root_task_id: String,
            pr_url: String,
        }
        let candidates = {
            let conn = self.connect()?;
            let mut stmt = conn.prepare(
                "SELECT a.id, s.root_task_id, s.canonical_pr_url
                 FROM pr_review_guide_attempts a
                 JOIN pr_review_guide_source_series s ON s.id = a.series_id
                 JOIN tasks t ON t.id = s.root_task_id
                 JOIN pr_review_guide_source_comparisons c ON c.id = a.comparison_id
                 WHERE a.status = 'failed'
                   AND a.failed_pre_start = 1
                   AND a.failed_by_build IS NOT NULL AND a.failed_by_build != ?1
                   AND s.selected_comparison_id = a.comparison_id
                   AND t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
                   AND t.pr_url = s.canonical_pr_url
                   AND NOT EXISTS (
                       SELECT 1 FROM pr_review_guide_attempts n
                       WHERE n.series_id = a.series_id AND n.request_epoch > a.request_epoch
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM pr_review_guide_versions v
                       JOIN pr_review_guide_source_comparisons published ON published.id = v.comparison_id
                       WHERE v.series_id = a.series_id AND published.head_sha = c.head_sha
                   )
                 ORDER BY a.finished_at, a.id",
            )?;
            stmt.query_map([running_build], |row| {
                Ok(Candidate {
                    attempt_id: row.get(0)?,
                    root_task_id: row.get(1)?,
                    pr_url: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut report = ReviewGuideReenqueueReport {
            build: running_build.to_owned(),
            reenqueued: Vec::new(),
        };
        for candidate in candidates {
            match pr_checker.check(&candidate.pr_url) {
                Ok(PrOpenState::Open) => {}
                Ok(PrOpenState::Merged | PrOpenState::ClosedUnmerged) => continue,
                Err(err) => {
                    tracing::warn!(
                        pr_url = %candidate.pr_url,
                        ?err,
                        "review-guide re-enqueue: could not confirm PR is open",
                    );
                    continue;
                }
            }
            // The token makes a repeat of this exact re-enqueue (a crash
            // between admit and dispatch, say) return the same attempt.
            let token = format!("build-change-reenqueue:{}", candidate.attempt_id);
            match self.request_pr_review_guide(
                &candidate.root_task_id,
                Some(&candidate.pr_url),
                Some(&token),
                boss_review_guide::PROMPT_VERSION,
                false,
            ) {
                Ok(RetryReviewGuideOutcome::Created(attempt)) => {
                    report.reenqueued.push(ReenqueuedReviewGuide {
                        root_task_id: candidate.root_task_id,
                        pr_url: candidate.pr_url,
                        failed_attempt_id: candidate.attempt_id,
                        new_attempt_id: attempt.id,
                    });
                }
                Ok(RetryReviewGuideOutcome::AlreadyRequested(_) | RetryReviewGuideOutcome::NoComparison) => {}
                Err(err) => tracing::warn!(
                    attempt_id = %candidate.attempt_id,
                    root_task_id = %candidate.root_task_id,
                    ?err,
                    "review-guide re-enqueue: could not re-enqueue a pre-start-failed attempt",
                ),
            }
        }
        Ok(report)
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

    /// Resolve the root task id a review-guide series belongs to. A
    /// `PrReviewGuide` execution's `work_item_id` is the comparison id, not
    /// a task id (see [`Self::review_guide_source_root_for_execution`] for
    /// the analogous execution-side resolution), so the completion
    /// finalizer needs this to get back to the owning board card from an
    /// attempt's `series_id` alone — including failure paths that never
    /// load the comparison.
    pub fn root_task_id_for_review_guide_series(&self, series_id: &str) -> Result<Option<String>> {
        self.root_task_id_for_series(series_id)
    }

    /// Series/comparison identity plus lifecycle for the `GetReviewGuide`
    /// RPC's summary half. `root_task_id` resolves the same way
    /// [`Self::get_latest_pr_review_guide_source_capture`] does.
    pub fn get_pr_review_guide_summary_for_root(&self, root_task_id: &str) -> Result<Option<PrReviewGuideSummary>> {
        let conn = self.connect()?;
        query_pr_review_guide_summary_for_root(&conn, root_task_id, None, None).map_err(Into::into)
    }

    /// Like [`Self::get_pr_review_guide_summary_for_root`], but when
    /// `series_id` is set, scopes the lookup to that specific series instead
    /// of whichever series on the root observed most recently. Used by the
    /// `GetReviewGuideSummary` RPC so a viewer polling an already-open guide
    /// keeps seeing that guide's own PR series even after a replacement PR
    /// creates a newer series on the same root task.
    pub fn get_pr_review_guide_summary_for_root_scoped(
        &self,
        root_task_id: &str,
        series_id: Option<&str>,
    ) -> Result<Option<PrReviewGuideSummary>> {
        let conn = self.connect()?;
        query_pr_review_guide_summary_for_root(&conn, root_task_id, None, series_id).map_err(Into::into)
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
        self.request_pr_review_guide(root_task_id, None, idempotency_token, prompt_version, true)
    }

    /// Manual generation reuses a live attempt for the selected comparison.
    /// Once terminal, a new token admits a fresh epoch, preserving versions.
    pub(crate) fn generate_pr_review_guide(
        &self,
        root_task_id: &str,
        pr_url: &str,
        idempotency_token: Option<&str>,
    ) -> Result<RetryReviewGuideOutcome> {
        self.request_pr_review_guide(
            root_task_id,
            Some(pr_url),
            idempotency_token,
            boss_review_guide::PROMPT_VERSION,
            false,
        )
    }

    /// The pending source-capture failure for a root's guide series, if any.
    pub(crate) fn pr_review_guide_capture_error_for_root(&self, root_task_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let Some(summary) = query_pr_review_guide_summary_for_root(&conn, root_task_id, None, None)? else {
            return Ok(None);
        };
        Ok(conn.query_row(
            "SELECT last_capture_error FROM pr_review_guide_source_series WHERE id = ?1",
            [&summary.series_id],
            |row| row.get(0),
        )?)
    }

    fn request_pr_review_guide(
        &self,
        root_task_id: &str,
        pr_url: Option<&str>,
        idempotency_token: Option<&str>,
        prompt_version: &str,
        replace: bool,
    ) -> Result<RetryReviewGuideOutcome> {
        let summary = {
            let conn = self.connect()?;
            query_pr_review_guide_summary_for_root(&conn, root_task_id, pr_url, None)?
        };
        let Some(summary) = summary else {
            return Ok(RetryReviewGuideOutcome::NoComparison);
        };
        let Some(comparison_id) = summary.selected_comparison_id else {
            return Ok(RetryReviewGuideOutcome::NoComparison);
        };
        let capture_error: Option<String> = self.connect()?.query_row(
            "SELECT last_capture_error FROM pr_review_guide_source_series WHERE id = ?1",
            [&summary.series_id],
            |row| row.get(0),
        )?;
        ensure!(
            capture_error.is_none(),
            "source capture failed: {}; the current head must be recaptured before retrying",
            capture_error.unwrap_or_default()
        );
        let (attempt, created) = self.admit_pr_review_guide_attempt(
            &summary.series_id,
            &comparison_id,
            prompt_version,
            idempotency_token,
            replace,
            // Retry resumes a failed revision update; Generate is a full run.
            replace,
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
}

const PR_REVIEW_GUIDE_ATTEMPT_COLUMNS: &str = "id, series_id, comparison_id, request_epoch, ordinal, execution_id, \
     status, prompt_version, driver, model, effort_value, error, retries, idempotency_token, created_at, started_at, finished_at, provider_usage_json, \
     failed_pre_start, failed_by_build";

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
        failed_pre_start: row.get::<_, i64>(18)? != 0,
        failed_by_build: row.get(19)?,
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

/// [`WorkDb::publish_pr_review_guide_version`] inside the caller's
/// transaction. Commits nothing: a caller that must publish atomically with
/// other writes commits only on [`PublishReviewGuideOutcome::Published`] and
/// drops the transaction otherwise.
pub(super) fn publish_pr_review_guide_version_in_tx(
    tx: &Connection,
    attempt_id: &str,
    markdown: &str,
    raw_output: &str,
    now: &str,
) -> Result<PublishReviewGuideOutcome> {
    let attempt = query_pr_review_guide_attempt(tx, attempt_id).require("pr_review_guide_attempt", attempt_id)?;
    if PrReviewGuideAttemptStatus::from_str(&attempt.status)?.is_terminal() {
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
    Ok(PublishReviewGuideOutcome::Published(Box::new(PrReviewGuideVersion {
        id: version_id,
        series_id: attempt.series_id,
        comparison_id: attempt.comparison_id,
        attempt_id: attempt_id.to_owned(),
        markdown: markdown.to_owned(),
        raw_output: raw_output.to_owned(),
        content_hash,
        prompt_version: attempt.prompt_version,
        generated_at: now.to_owned(),
    })))
}

/// Reverse the inferred-death closure of the attempt bound to a
/// `pr_review_guide` execution, inside the caller's transaction.
///
/// The orphan reap (`WorkDb::mark_execution_orphaned`) and the reconcile
/// pass fail an attempt purely on the strength of its execution's status.
/// When re-adoption later disproves that status
/// (`WorkDb::readopt_inferred_terminal_execution`), the attempt has to come
/// back with it, or the live worker has nothing to submit its guide to. The
/// same reversal lets a guide submitted while the attempt was wrongly closed
/// be recovered (`crate::review_guide_validation::recover_orphaned_submissions`).
///
/// Only an attempt that failed after a worker ran (`failed_pre_start = 0`),
/// whose comparison is still the series' selected one at the same request
/// epoch, with no guide published for it and no other live attempt in the
/// series, is reopened. With `accept_running`, an already-running attempt
/// (re-adoption got there first) passes the same checks and is re-stamped.
/// A pre-start failure, a superseded attempt, a published one, or one whose
/// series has moved on stands as recorded.
///
/// Returns the reopened attempt id.
pub(super) fn reopen_review_guide_attempt_for_readoption(
    tx: &Connection,
    execution_id: &str,
    now: &str,
    accept_running: bool,
) -> Result<Option<String>> {
    let attempt: Option<(String, String, String, i64, i64)> = tx
        .query_row(
            "SELECT id, series_id, comparison_id, request_epoch, failed_pre_start
             FROM pr_review_guide_attempts
             WHERE execution_id = ?1 AND (status = ?2 OR (?3 AND status = ?4))",
            params![
                execution_id,
                PrReviewGuideAttemptStatus::Failed.as_str(),
                accept_running,
                PrReviewGuideAttemptStatus::Running.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let Some((attempt_id, series_id, comparison_id, request_epoch, failed_pre_start)) = attempt else {
        return Ok(None);
    };
    if failed_pre_start != 0 {
        return Ok(None);
    }
    let (selected, epoch): (Option<String>, i64) = tx.query_row(
        "SELECT selected_comparison_id, request_epoch FROM pr_review_guide_source_series WHERE id = ?1",
        [&series_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if selected.as_deref() != Some(comparison_id.as_str()) || epoch != request_epoch {
        return Ok(None);
    }
    let blocked: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pr_review_guide_versions WHERE attempt_id = ?1)
             OR EXISTS(
                 SELECT 1 FROM pr_review_guide_attempts
                 WHERE series_id = ?2 AND id != ?1
                   AND status NOT IN ('succeeded', 'failed', 'cancelled', 'superseded')
             )",
        params![attempt_id, series_id],
        |row| row.get(0),
    )?;
    if blocked {
        return Ok(None);
    }
    tx.execute(
        "UPDATE pr_review_guide_attempts
         SET status = ?2, error = NULL, finished_at = NULL, failed_by_build = NULL
         WHERE id = ?1",
        params![attempt_id, PrReviewGuideAttemptStatus::Running.as_str()],
    )?;
    tx.execute(
        "UPDATE pr_review_guide_source_series
         SET guide_lifecycle = 'generating', updated_at = ?2
         WHERE id = ?1 AND guide_lifecycle = 'failed'",
        params![series_id, now],
    )?;
    Ok(Some(attempt_id))
}

/// The query body behind [`WorkDb::get_pr_review_guide_summary_for_root`],
/// factored out to a `&Connection` so a future caller already holding one
/// open (a board/task read inside a transaction) is not forced to open a
/// second one just for this lookup.
fn query_pr_review_guide_summary_for_root(
    conn: &Connection,
    root_task_id: &str,
    pr_url: Option<&str>,
    series_id: Option<&str>,
) -> rusqlite::Result<Option<PrReviewGuideSummary>> {
    conn.query_row(
        &format!(
            "SELECT s.id, s.root_task_id, s.canonical_pr_url, s.guide_lifecycle, s.request_epoch,
                s.selected_comparison_id, s.readable_version_id,
                {REVIEW_GUIDE_LATEST_TERMINAL_ERROR_SQL}
         FROM pr_review_guide_source_series s
         WHERE s.root_task_id = ?1 AND (?2 IS NULL OR s.canonical_pr_url = ?2) AND (?3 IS NULL OR s.id = ?3)
         ORDER BY s.latest_observation_sequence DESC, s.id DESC LIMIT 1"
        ),
        params![root_task_id, pr_url, series_id],
        |row| {
            Ok(PrReviewGuideSummary {
                series_id: row.get(0)?,
                root_task_id: row.get(1)?,
                canonical_pr_url: row.get(2)?,
                lifecycle: row.get(3)?,
                request_epoch: row.get(4)?,
                selected_comparison_id: row.get(5)?,
                readable_version_id: row.get(6)?,
                error: row.get(7)?,
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
        .maybe_error(summary.error)
        .build()
}

#[cfg(test)]
#[path = "review_guide_jobs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "review_guide_reenqueue_tests.rs"]
mod reenqueue_tests;

/// Immutable starting points for incremental guide updates and revision attribution.
pub(in crate::work) mod guide_updates {
    use super::*;

    pub(in crate::work) fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pr_review_guide_updates (
            attempt_id TEXT PRIMARY KEY REFERENCES pr_review_guide_attempts(id) ON DELETE CASCADE,
            previous_version_id TEXT NOT NULL REFERENCES pr_review_guide_versions(id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS pr_review_guide_automatic_attempts (
            attempt_id TEXT PRIMARY KEY REFERENCES pr_review_guide_attempts(id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS pr_review_guide_revision_heads (
            root_task_id TEXT NOT NULL,
            pr_url TEXT NOT NULL,
            head_sha TEXT NOT NULL,
            revision_task_id TEXT NOT NULL,
            observation_sequence INTEGER NOT NULL,
            PRIMARY KEY(root_task_id, pr_url, head_sha)
         );
         CREATE TABLE IF NOT EXISTS pr_review_guide_head_ancestry (
            root_task_id TEXT NOT NULL,
            pr_url TEXT NOT NULL,
            parent_sha TEXT NOT NULL,
            child_sha TEXT NOT NULL,
            PRIMARY KEY(root_task_id, pr_url, parent_sha, child_sha)
         );",
        )?;
        Ok(())
    }

    /// True for an automatic (revision-triggered or retried) attempt that would
    /// replace a readable guide while a revision in the root's chain is still
    /// actively writing; the replacement waits for that writer to finish. The
    /// root is resolved from the attempt's series, so an attempt admitted before
    /// completion attribution arrives is held too. Parked (todo/blocked)
    /// revisions are not writing and never hold an update.
    pub(in crate::work) fn chain_has_active_writer(conn: &Connection, attempt: &str) -> Result<bool> {
        let root: Option<String> = conn
            .query_row(
                "SELECT s.root_task_id FROM pr_review_guide_attempts a
                 JOIN pr_review_guide_source_series s ON s.id = a.series_id
                 JOIN pr_review_guide_source_comparisons new ON new.id = a.comparison_id
                 JOIN pr_review_guide_versions v ON v.id = s.readable_version_id
                 JOIN pr_review_guide_source_comparisons old ON old.id = v.comparison_id
                 WHERE a.id = ?1 AND old.head_sha != new.head_sha
                   AND (EXISTS (SELECT 1 FROM pr_review_guide_updates u WHERE u.attempt_id = a.id)
                     OR EXISTS (SELECT 1 FROM pr_review_guide_automatic_attempts m WHERE m.attempt_id = a.id))",
                [attempt],
                |row| row.get(0),
            )
            .optional()?;
        let Some(root) = root else { return Ok(false) };
        for id in chain_helpers::collect_chain_revision_ids_including_deleted(conn, &root)? {
            if let Some(task) = query_task(conn, &id)?
                && task.deleted_at.is_none()
                && task.status == TaskStatus::Active
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Snapshot the readable version in the admission transaction. Superseding a
    /// queued update still starts from the published version, never unpublished prose.
    pub(in crate::work) fn snapshot(conn: &Connection, attempt: &str, series: &str, comparison: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO pr_review_guide_updates (attempt_id, previous_version_id)
         SELECT ?1, v.id FROM pr_review_guide_source_series s
         JOIN pr_review_guide_versions v ON v.id = s.readable_version_id
         JOIN pr_review_guide_source_comparisons old ON old.id = v.comparison_id
         JOIN pr_review_guide_source_comparisons new ON new.id = ?3
         WHERE s.id = ?2 AND old.head_sha != new.head_sha
           AND EXISTS (SELECT 1 FROM pr_review_guide_revision_heads r
             WHERE r.root_task_id = s.root_task_id AND r.pr_url = s.canonical_pr_url
               AND r.head_sha = new.head_sha)",
            params![attempt, series, comparison],
        )?;
        conn.execute(
            "UPDATE pr_review_guide_attempts SET prompt_version = ?2
         WHERE id = ?1 AND EXISTS (SELECT 1 FROM pr_review_guide_updates WHERE attempt_id = ?1)",
            params![attempt, boss_review_guide::UPDATE_PROMPT_VERSION],
        )?;
        Ok(())
    }

    impl WorkDb {
        /// True when the comparison already has live work or its guide is the one
        /// currently readable. A historical version for a head the PR has since
        /// left (A -> B -> A) does not count: the card would keep opening B.
        pub(crate) fn review_guide_comparison_has_attempt(&self, comparison: &str) -> Result<bool> {
            Ok(self.connect()?.query_row(
                "SELECT EXISTS(SELECT 1 FROM pr_review_guide_attempts
                 WHERE comparison_id = ?1 AND status IN ('queued', 'running'))
                 OR EXISTS(SELECT 1 FROM pr_review_guide_source_series s
                 JOIN pr_review_guide_versions v ON v.id = s.readable_version_id
                 WHERE v.comparison_id = ?1)",
                [comparison],
                |row| row.get(0),
            )?)
        }

        /// The old head and original Markdown are immutable and bound to this attempt.
        pub(crate) fn review_guide_update_context(&self, execution: &str) -> Result<Option<(String, String)>> {
            let conn = self.connect()?;
            let context = conn
                .query_row(
                    "SELECT c.head_sha, v.markdown FROM pr_review_guide_updates u
             JOIN pr_review_guide_attempts a ON a.id = u.attempt_id
             JOIN pr_review_guide_versions v ON v.id = u.previous_version_id
             JOIN pr_review_guide_source_comparisons c ON c.id = v.comparison_id
             WHERE a.execution_id = ?1",
                    [execution],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let requires_update: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pr_review_guide_attempts WHERE execution_id = ?1 AND prompt_version = ?2)",
                params![execution, boss_review_guide::UPDATE_PROMPT_VERSION],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                !requires_update || context.is_some(),
                "incremental guide starting version is missing"
            );
            Ok(context)
        }

        /// A revision pushed `child` on top of `parent`, so `child` contains
        /// `parent`. A force-push never records an edge, which is what keeps a
        /// replaced head from counting as contained.
        pub(crate) fn record_review_guide_head_ancestry(
            &self,
            root: &str,
            pr: &str,
            parent: &str,
            child: &str,
        ) -> Result<()> {
            self.connect()?.execute(
                "INSERT OR IGNORE INTO pr_review_guide_head_ancestry
                 (root_task_id, pr_url, parent_sha, child_sha) VALUES (?1, ?2, ?3, ?4)",
                params![root, pr, parent, child],
            )?;
            Ok(())
        }

        pub(crate) fn record_review_guide_revision_head(
            &self,
            root: &str,
            pr: &str,
            head: &str,
            revision: &str,
            sequence: i64,
        ) -> Result<()> {
            self.connect()?.execute(
                "INSERT INTO pr_review_guide_revision_heads
             (root_task_id, pr_url, head_sha, revision_task_id, observation_sequence)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_task_id, pr_url, head_sha) DO UPDATE SET
               revision_task_id = excluded.revision_task_id,
               observation_sequence = excluded.observation_sequence
             WHERE excluded.observation_sequence > observation_sequence",
                params![root, pr, head, revision, sequence],
            )?;
            // The poller may have admitted (and held) the attempt before this
            // attribution arrived: turn it into the incremental update.
            let conn = self.connect()?;
            let held = {
                let mut stmt = conn.prepare(
                    "SELECT a.id, a.series_id, a.comparison_id FROM pr_review_guide_attempts a
                     JOIN pr_review_guide_automatic_attempts m ON m.attempt_id = a.id
                     JOIN pr_review_guide_source_series s ON s.id = a.series_id
                     JOIN pr_review_guide_source_comparisons c ON c.id = a.comparison_id
                     WHERE s.root_task_id = ?1 AND s.canonical_pr_url = ?2 AND c.head_sha = ?3
                       AND a.status = 'queued' AND a.execution_id IS NULL
                       AND NOT EXISTS (SELECT 1 FROM pr_review_guide_updates u WHERE u.attempt_id = a.id)",
                )?;
                stmt.query_map(params![root, pr, head], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (attempt, series, comparison) in held {
                snapshot(&conn, &attempt, &series, &comparison)?;
            }
            Ok(())
        }
    }

    /// Shared by board reads and the series-scoped viewer poll. A revision label
    /// belongs to an exact head, never to a replacement PR.
    pub(in crate::work) fn status(conn: &Connection, root: &str, pr: &str) -> Result<Option<String>> {
        let row = conn
            .query_row(
                "SELECT s.guide_lifecycle, c.head_sha, t.id, t.short_id
         FROM pr_review_guide_source_series s
         JOIN pr_review_guide_source_comparisons c ON c.id = s.selected_comparison_id
         JOIN pr_review_guide_revision_heads r ON r.root_task_id = s.root_task_id
           AND r.pr_url = s.canonical_pr_url
           AND ((s.last_capture_error IS NULL AND r.head_sha = c.head_sha)
             OR (s.last_capture_error IS NOT NULL AND r.observation_sequence = s.latest_observation_sequence))
         JOIN tasks t ON t.id = r.revision_task_id
         WHERE s.root_task_id = ?1 AND s.canonical_pr_url = ?2
           AND s.readable_version_id IS NOT NULL
           AND (s.last_capture_error IS NOT NULL OR EXISTS (SELECT 1 FROM pr_review_guide_attempts a
             JOIN pr_review_guide_updates u ON u.attempt_id = a.id
             WHERE a.series_id = s.id AND a.request_epoch = s.request_epoch
               AND a.comparison_id = s.selected_comparison_id))
         ORDER BY r.observation_sequence DESC LIMIT 1",
                params![root, pr],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(|(lifecycle, head, id, short)| {
            let label = boss_protocol::short_id_label(short).unwrap_or(id);
            match lifecycle.as_str() {
                "queued" | "generating" => format!("Updating after revision {label}..."),
                "ready" => format!("Updated for revision {label} at {head}"),
                _ => format!("Update after revision {label} failed; previous guide retained"),
            }
        }))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_support::{
            create_active_chore, create_product, open_db, review_guide_source_packet, seed_review_guide_series,
        };

        #[test]
        fn retention_preserves_a_live_updates_starting_version_then_reclaims_terminal_history() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "guide retention");
            let (series, comparison) = seed_review_guide_series(&db, &root);
            let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            let PublishReviewGuideOutcome::Published(old) = db
                .publish_pr_review_guide_version(&first.id, "# Original", "original")
                .unwrap()
            else {
                panic!("publish");
            };
            let PrSourceCapturePersistOutcome::Stored(new) = db
                .persist_pr_review_guide_source_capture(
                    &root,
                    2,
                    PrSourceCaptureTrigger::Completion,
                    &review_guide_source_packet("base", "new-head"),
                )
                .unwrap()
            else {
                panic!("capture");
            };
            let url = db
                .get_pr_review_guide_summary_for_root(&root)
                .unwrap()
                .unwrap()
                .canonical_pr_url;
            db.record_review_guide_revision_head(&root, &url, "new-head", "revision", 2)
                .unwrap();
            let update = db
                .create_automatic_pr_review_guide_attempt(&series, &new.comparison_id)
                .unwrap();
            assert_eq!(update.prompt_version, boss_review_guide::UPDATE_PROMPT_VERSION);
            let conn = db.connect().unwrap();
            conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&root])
                .unwrap();
            conn.execute("UPDATE pr_review_guide_versions SET generated_at = '1'", [])
                .unwrap();
            conn.execute("UPDATE pr_review_guide_source_comparisons SET captured_at = '1'", [])
                .unwrap();
            conn.execute("UPDATE pr_review_guide_attempts SET created_at = '1'", [])
                .unwrap();
            drop(conn);
            db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
            assert!(db.get_pr_review_guide_version(&old.id).unwrap().is_some());
            assert_eq!(
                db.get_pr_review_guide_summary_for_root(&root)
                    .unwrap()
                    .unwrap()
                    .readable_version_id,
                Some(old.id.clone())
            );
            db.fail_pr_review_guide_attempt(&update.id, "failed").unwrap();
            db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
            // A failed update keeps the previous version readable.
            assert!(db.get_pr_review_guide_version(&old.id).unwrap().is_some());
            let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
            assert_eq!(summary.lifecycle, "failed");
            assert_eq!(summary.readable_version_id, Some(old.id.clone()));
            // The failed request and its comparison survive GC, so the status
            // text and Retry still have something to act on.
            assert_eq!(summary.selected_comparison_id, Some(new.comparison_id.clone()));
            assert!(summary.error.is_some());
            let RetryReviewGuideOutcome::Created(retry) = db
                .retry_pr_review_guide(&root, None, boss_review_guide::PROMPT_VERSION)
                .unwrap()
            else {
                panic!("retry");
            };
            assert_eq!(retry.prompt_version, boss_review_guide::UPDATE_PROMPT_VERSION);
            // Retried to success: the new guide is readable and survives GC; the
            // superseded old version is no longer protected and may be reclaimed.
            let PublishReviewGuideOutcome::Published(newer) = db
                .publish_pr_review_guide_version(&retry.id, "# Updated", "updated")
                .unwrap()
            else {
                panic!("publish");
            };
            db.connect()
                .unwrap()
                .execute("UPDATE pr_review_guide_versions SET generated_at = '1'", [])
                .unwrap();
            db.gc_unreferenced_pr_review_guide_source_artifacts().unwrap();
            assert!(db.get_pr_review_guide_version(&newer.id).unwrap().is_some());
            let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
            assert_eq!(summary.lifecycle, "ready");
            assert_eq!(summary.readable_version_id, Some(newer.id));
        }

        #[test]
        fn explicit_generate_is_never_an_incremental_update() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "manual generate");
            let (series, comparison) = seed_review_guide_series(&db, &root);
            let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            db.publish_pr_review_guide_version(&first.id, "# Original", "original")
                .unwrap();
            let PrSourceCapturePersistOutcome::Stored(new) = db
                .persist_pr_review_guide_source_capture(
                    &root,
                    2,
                    PrSourceCaptureTrigger::Completion,
                    &review_guide_source_packet("base", "new-head"),
                )
                .unwrap()
            else {
                panic!("capture");
            };
            let url = db
                .get_pr_review_guide_summary_for_root(&root)
                .unwrap()
                .unwrap()
                .canonical_pr_url;
            db.record_review_guide_revision_head(&root, &url, "new-head", "revision", 2)
                .unwrap();
            let manual = db
                .create_pr_review_guide_attempt_with_token(&series, &new.comparison_id, "test", Some("click"))
                .unwrap();
            assert_ne!(manual.prompt_version, boss_review_guide::UPDATE_PROMPT_VERSION);
            let updates: i64 = db
                .connect()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM pr_review_guide_updates WHERE attempt_id = ?1",
                    [&manual.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(updates, 0, "manual generation after a head change is a full run");
        }

        #[test]
        fn failed_or_cancelled_attempts_do_not_suppress_automatic_enqueue() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "failed attempt");
            let (series, comparison) = seed_review_guide_series(&db, &root);
            let attempt = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            assert!(db.review_guide_comparison_has_attempt(&comparison).unwrap());
            db.fail_pr_review_guide_attempt(&attempt.id, "failed").unwrap();
            assert!(!db.review_guide_comparison_has_attempt(&comparison).unwrap());
            let retry = db
                .create_automatic_pr_review_guide_attempt(&series, &comparison)
                .unwrap();
            assert_ne!(retry.id, attempt.id);
        }

        #[test]
        fn failed_source_capture_keeps_published_guide_and_fences_old_jobs() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "capture failure");
            let (series, comparison) = seed_review_guide_series(&db, &root);
            let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            let PublishReviewGuideOutcome::Published(old) = db
                .publish_pr_review_guide_version(&first.id, "# Original", "original")
                .unwrap()
            else {
                panic!("publish");
            };
            let stale = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            let url = db
                .get_pr_review_guide_summary_for_root(&root)
                .unwrap()
                .unwrap()
                .canonical_pr_url;
            // The failed observation concerns a revision head the guide does not cover.
            db.record_review_guide_revision_head(&root, &url, "new-head", "revision", 10)
                .unwrap();
            db.record_pr_review_guide_source_capture_failure(&root, &url, 10, "pinned delta unavailable")
                .unwrap();
            assert_eq!(
                db.publish_pr_review_guide_version(&stale.id, "# Late", "late").unwrap(),
                PublishReviewGuideOutcome::Superseded
            );
            let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
            assert_eq!(summary.lifecycle, "failed");
            assert_eq!(summary.readable_version_id, Some(old.id));
            assert!(
                db.retry_pr_review_guide(&root, None, "test")
                    .unwrap_err()
                    .to_string()
                    .contains("recapture")
            );
        }

        #[test]
        fn transient_capture_failure_on_the_published_head_keeps_the_guide_ready() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "transient failure");
            let (series, comparison) = seed_review_guide_series(&db, &root);
            let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            db.publish_pr_review_guide_version(&first.id, "# Original", "original")
                .unwrap();
            let live = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            let url = db
                .get_pr_review_guide_summary_for_root(&root)
                .unwrap()
                .unwrap()
                .canonical_pr_url;
            // A metadata error on a recheck of the unchanged head: no revision
            // attribution for another head, so nothing is failed or fenced.
            db.record_pr_review_guide_source_capture_failure(&root, &url, 10, "gh metadata unavailable")
                .unwrap();
            let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
            assert_eq!(summary.lifecycle, "queued");
            assert!(db.pr_review_guide_capture_error_for_root(&root).unwrap().is_some());
            assert!(matches!(
                db.publish_pr_review_guide_version(&live.id, "# Regenerated", "raw")
                    .unwrap(),
                PublishReviewGuideOutcome::Published(_)
            ));
            // The next successful capture of the same comparison clears the error.
            db.persist_pr_review_guide_source_capture(
                &root,
                11,
                PrSourceCaptureTrigger::Completion,
                &review_guide_source_packet("base", "head"),
            )
            .unwrap();
            let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
            assert_eq!(summary.lifecycle, "ready");
            assert!(db.pr_review_guide_capture_error_for_root(&root).unwrap().is_none());
        }

        #[test]
        fn recapturing_the_published_comparison_restores_a_failed_guide_to_ready() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "restore ready");
            let (series, comparison) = seed_review_guide_series(&db, &root);
            let first = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
            db.publish_pr_review_guide_version(&first.id, "# Original", "original")
                .unwrap();
            let url = db
                .get_pr_review_guide_summary_for_root(&root)
                .unwrap()
                .unwrap()
                .canonical_pr_url;
            db.record_review_guide_revision_head(&root, &url, "new-head", "revision", 10)
                .unwrap();
            db.record_pr_review_guide_source_capture_failure(&root, &url, 10, "pinned delta unavailable")
                .unwrap();
            assert_eq!(
                db.get_pr_review_guide_summary_for_root(&root)
                    .unwrap()
                    .unwrap()
                    .lifecycle,
                "failed"
            );
            // The PR head went back to the head the guide already covers.
            db.persist_pr_review_guide_source_capture(
                &root,
                11,
                PrSourceCaptureTrigger::Poller,
                &review_guide_source_packet("base", "head"),
            )
            .unwrap();
            let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
            assert_eq!(summary.lifecycle, "ready");
            assert!(db.pr_review_guide_capture_error_for_root(&root).unwrap().is_none());
        }
    }
}
