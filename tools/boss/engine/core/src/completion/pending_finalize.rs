//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]; see [`super`] for the handler struct and the
//! shared types this module reaches via `use super::*`.
//!
//! ## Why this module exists
//!
//! An accepted `run_done` declaration must reach a terminal write without the
//! worker doing anything further. Two incidents showed that a single
//! synchronous attempt at submission is not enough: when that attempt's DB
//! write fails (a locked database) or GitHub cannot answer, the declaration is
//! durably stamped, the execution stays live, and — with the worker idle and
//! no further turn-end coming — nothing ever finalizes it.
//!
//! Three layers own the retry, each ending only at a terminal commit:
//!
//! 1. [`WorkerCompletionHandler::finalize_declared_run_done_until_resolved`]
//!    — the fast, in-process loop `app::proposals` spawns when the submission
//!    attempt does not resolve.
//! 2. [`WorkerCompletionHandler::sweep_pending_finalizes`] — the durable
//!    backstop run by [`crate::run_done_finalize_sweep`]. It re-derives the
//!    work from the stamp itself, so it survives an engine restart or a
//!    panicked retry task.
//! 3. The retained `no-changes-needed` write ([`PendingNoOpCompletion`]):
//!    that outcome is accepted only at the driver's terminal boundary, so a
//!    failed write there is retained (already-validated inputs, declaration
//!    identity re-checked by the write itself) and retried by the same sweep.
//!
//! There is deliberately no attempt cap — none has been measured. A stuck
//! finalize stays visible through [`RUN_DONE_FINALIZE_RETRY`] and an
//! execution attention item rather than being silently abandoned.

use std::time::{Duration, Instant};

use super::*;

const RETRY_INITIAL_DELAY: Duration = Duration::from_millis(500);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

/// How long a finalize may keep failing before an attention item is raised.
/// Not derived from a measurement (none exists): it is a few multiples of the
/// backoff ceiling plus the sweep cadence, i.e. long enough that routine
/// SQLite contention or a short GitHub blip has cleared.
pub(crate) const STUCK_FINALIZE_ATTENTION_AFTER: Duration = Duration::from_secs(10 * 60);

/// Attention kind filed when a declared finalize is stuck.
pub const RUN_DONE_FINALIZE_STUCK_ATTENTION_KIND: &str = "run_done_finalize_stuck";

/// Result of one [`WorkerCompletionHandler::sweep_pending_finalizes`] pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PendingFinalizeSweepOutcome {
    /// Stamped delivered/blocked declarations that reached a terminal state.
    pub finalized: usize,
    /// Stamped declarations still live after this pass's attempt.
    pub still_pending: usize,
    /// Retained no-op completions that reached a terminal state.
    pub no_ops_finalized: usize,
    /// Retained no-op completions still failing.
    pub no_ops_still_pending: usize,
}

/// Whether a finalize attempt ended the declaration's life: anything except a
/// DB failure or an inconclusive GitHub answer (both leave the execution live
/// with its stamp) or a deferred no-op.
pub fn finalize_resolved(outcome: &StopOutcome) -> bool {
    !matches!(
        outcome,
        StopOutcome::DbError | StopOutcome::DetectorFailed | StopOutcome::AwaitingInput
    )
}

impl WorkerCompletionHandler {
    /// [`Self::finalize_declared_run_done`], repeated until it reaches a
    /// terminal commit: a DB failure ([`StopOutcome::DbError`]) or an
    /// inconclusive GitHub answer ([`StopOutcome::DetectorFailed`]) both
    /// leave the execution live with its declaration stamped, so both retry.
    /// Every failed attempt bumps [`RUN_DONE_FINALIZE_RETRY`] and is logged; a
    /// finalize still failing after [`STUCK_FINALIZE_ATTENTION_AFTER`] raises
    /// an attention item. Only the delay between attempts is bounded.
    pub async fn finalize_declared_run_done_until_resolved(
        &self,
        execution_id: &str,
        outcome: boss_protocol::RunDoneOutcome,
    ) -> StopOutcome {
        let started = Instant::now();
        let mut attempt = 0u32;
        let mut attention_filed = false;
        loop {
            let result = self.finalize_declared_run_done(execution_id, outcome).await;
            if finalize_resolved(&result) {
                return result;
            }
            RUN_DONE_FINALIZE_RETRY.inc(&self.metrics);
            let delay = boss_backoff::exponential_delay(RETRY_INITIAL_DELAY, RETRY_MAX_DELAY, attempt);
            attempt = attempt.saturating_add(1);
            tracing::error!(
                execution_id,
                attempt,
                ?result,
                retry_in_ms = delay.as_millis() as u64,
                "run_done finalize did not reach a terminal commit; the declaration is still stamped, retrying",
            );
            if !attention_filed && started.elapsed() >= STUCK_FINALIZE_ATTENTION_AFTER {
                attention_filed = true;
                self.file_stuck_finalize_attention(execution_id, attempt, started.elapsed())
                    .await;
            }
            tokio::time::sleep(delay).await;
        }
    }

    async fn file_stuck_finalize_attention(&self, execution_id: &str, attempts: u32, elapsed: Duration) {
        let Ok(execution) = self.work_db.get_execution(execution_id) else {
            return;
        };
        if !execution.status.is_live() {
            return;
        }
        let body = format!(
            "Execution `{execution_id}` declared itself done, but the engine has failed to finalize it \
             {attempts} times over {} minutes (a database write error or an inconclusive GitHub answer). \
             The run still holds its worker slot and cube lease. The engine keeps retrying; check the \
             engine log for `run_done finalize did not reach a terminal commit`.",
            elapsed.as_secs() / 60
        );
        if let Err(err) = self
            .file_execution_attention(
                &execution,
                RUN_DONE_FINALIZE_STUCK_ATTENTION_KIND,
                "A declared-done run cannot be finalized",
                body,
            )
            .await
        {
            tracing::warn!(
                execution_id,
                ?err,
                "run_done finalize: failed to file the stuck-finalize attention"
            );
        }
    }

    /// One durable recovery pass: re-run the finalize for every live
    /// execution whose `delivered` / `blocked` declaration was stamped at or
    /// before `declared_at_or_before` (epoch seconds; the grace keeps the pass
    /// off the submission path's own in-flight finalize and ack delivery), and
    /// retry every retained `no-changes-needed` write. Each attempt takes the
    /// per-execution boundary lock and is idempotent.
    pub async fn sweep_pending_finalizes(&self, declared_at_or_before: i64) -> PendingFinalizeSweepOutcome {
        let mut outcome = PendingFinalizeSweepOutcome::default();
        if self.feature_flags.is_enabled("worker_proposals") && self.feature_flags.is_enabled("run_done_proposals_seam")
        {
            match self
                .work_db
                .live_executions_with_pending_run_done(declared_at_or_before)
            {
                Ok(pending) => {
                    for (execution_id, declared) in pending {
                        let result = self.finalize_declared_run_done(&execution_id, declared).await;
                        if finalize_resolved(&result) {
                            outcome.finalized += 1;
                            tracing::info!(%execution_id, ?result, "run_done finalize sweep: recovered a stamped declaration");
                        } else {
                            RUN_DONE_FINALIZE_RETRY.inc(&self.metrics);
                            outcome.still_pending += 1;
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(?err, "run_done finalize sweep: pending-declaration query failed")
                }
            }
        }
        let (finalized, pending) = self.retry_pending_no_op_completions().await;
        outcome.no_ops_finalized = finalized;
        outcome.no_ops_still_pending = pending;
        outcome
    }

    /// Terminal write of a `no-changes-needed` completion that retains its
    /// inputs when the write fails, so [`Self::sweep_pending_finalizes`] can
    /// finish it with no further worker event.
    pub(super) async fn finalize_no_op_completion_retaining(
        &self,
        execution: &crate::work::WorkExecution,
        contribution: Option<ContributionEvidence>,
        attention: Option<CreateAttentionItemInput>,
    ) -> StopOutcome {
        let outcome = self
            .finalize_no_op_completion(execution, contribution, attention.clone())
            .await;
        let mut pending = self.pending_no_op_completions.lock().expect("pending no-op poisoned");
        if outcome == StopOutcome::DbError {
            RUN_DONE_FINALIZE_RETRY.inc(&self.metrics);
            tracing::error!(
                execution_id = %execution.id,
                "no-op completion write failed; retained for background retry",
            );
            pending.insert(
                execution.id.clone(),
                PendingNoOpCompletion {
                    contribution,
                    attention,
                },
            );
        } else {
            pending.remove(&execution.id);
        }
        outcome
    }

    /// Retry the retained no-op writes; returns `(finalized, still pending)`.
    /// The write re-reads the applied declaration's identity inside its own
    /// transaction, so a declaration consumed or replaced since the Stop
    /// boundary ends the retry instead of completing on stale authority.
    async fn retry_pending_no_op_completions(&self) -> (usize, usize) {
        let ids: Vec<String> = self
            .pending_no_op_completions
            .lock()
            .expect("pending no-op poisoned")
            .keys()
            .cloned()
            .collect();
        let (mut finalized, mut still_pending) = (0, 0);
        for execution_id in ids {
            let lock = self.boundary_lock(&execution_id);
            let _boundary = lock.lock().await;
            let pending = self
                .pending_no_op_completions
                .lock()
                .expect("pending no-op poisoned")
                .get(&execution_id)
                .cloned();
            let Some(pending) = pending else {
                continue;
            };
            let execution = match self.work_db.get_execution(&execution_id) {
                Ok(execution) if execution.status.is_live() => execution,
                Ok(_) => {
                    self.pending_no_op_completions
                        .lock()
                        .expect("pending no-op poisoned")
                        .remove(&execution_id);
                    continue;
                }
                Err(err) => {
                    tracing::error!(%execution_id, ?err, "no-op retry: execution lookup failed");
                    still_pending += 1;
                    continue;
                }
            };
            let result = self
                .finalize_no_op_completion_retaining(&execution, pending.contribution, pending.attention)
                .await;
            if result == StopOutcome::DbError {
                still_pending += 1;
            } else {
                finalized += 1;
            }
        }
        (finalized, still_pending)
    }
}
