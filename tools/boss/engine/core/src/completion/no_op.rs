//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Finalize a sanctioned no-op completion: the worker verified its work
    /// is already done (empty diff, no PR produced and none bound), so the
    /// task is closed cleanly as `done` WITHOUT a PR and the execution is
    /// finalised. No nudge is sent. Mirrors [`Self::finalize_pr_transition`]'s
    /// lease/pane release and event publishing, but never stamps a `pr_url`
    /// (there is none — fabricating one is the empty-PR the worker refused).
    ///
    /// Idempotent against an already-finalized execution: the DB write
    /// returns `None` for a non-live row, which maps to `AlreadyTerminal`.
    pub(super) async fn finalize_no_op_completion(&self, execution: &crate::work::WorkExecution) -> StopOutcome {
        // Captured before `record_worker_no_op_completion` below nulls
        // `workspace_path` in the same transaction that terminalizes the
        // execution — this path terminalizes a parked-live execution, so it
        // owns driver teardown.
        let workspace_path = execution.workspace_path.clone();
        let detail = "Worker verified the assigned work was already done (empty diff — no changes \
                      needed); closed as a no-op without a PR.";
        let completion = match self.work_db.record_worker_no_op_completion(&execution.id, detail) {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    ?err,
                    "no-op completion: failed to record",
                );
                return StopOutcome::DbError;
            }
        };
        // The worker reached a clean terminal — drop any staged URL and reset
        // the nudge counter so nothing lingers for this finalized execution.
        self.staged_pr_urls.forget(&execution.id);
        self.nudge_breaker.forget(&execution.id);
        self.build_wait_tracker.forget(&execution.id);
        self.background_children_tracker.forget(&execution.id);
        self.hold_registry.release(&execution.id);
        crate::driver_teardown::teardown_driver_workspace(
            &self.work_db,
            &execution.id,
            workspace_path.as_deref().map(std::path::Path::new),
        )
        .await;
        if let Some(lease_id) = completion.released_lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await
        {
            tracing::error!(
                execution_id = %execution.id,
                lease_id,
                ?err,
                "no-op completion: cube release failed"
            );
        }
        self.pane_releaser.release_pane(&execution.id).await;
        let work_item_id = completion.execution.work_item_id.clone();
        self.publisher
            .publish(
                &completion.execution.id,
                &work_item_id,
                completion.execution.status.as_str(),
                "worker_no_op_completed",
            )
            .await;
        let product_id = completion.work_item.product_id().to_string();
        self.publisher
            .publish_work_item_changed(&product_id, &work_item_id, "worker_no_op_completed")
            .await;
        tracing::info!(
            execution_id = %execution.id,
            work_item_id = %work_item_id,
            kind = %execution.kind,
            "no-op completion: task closed as done without a PR (work already done)"
        );
        StopOutcome::NoChangesNeeded { work_item_id }
    }

    /// Finalize an execution whose driver reported its own terminal turn
    /// boundary as an unrecoverable error (see
    /// [`StopOutcome::DriverTerminalError`]). The worker process that
    /// produced this Stop has already exited — there is nothing left to
    /// nudge, wait on, or reconstruct a decision from. Marks the execution
    /// `failed`, releases its cube lease and pane, and files a human-visible
    /// attention item naming the provider's own diagnostic.
    ///
    /// Deliberately parallel to [`Self::finalize_no_op_completion`]'s
    /// teardown mechanics but with `failed` (not `completed`) status: the
    /// caller — [`crate::completion::stop`]'s early gate in `on_stop_inner`
    /// — runs this BEFORE any kind-specific finalizer (automation triage,
    /// answer-agent, pr_review, the generic nudge loop), so a driver-reported
    /// fatal error can never reach the nudge path that used to re-prompt an
    /// already-dead process.
    ///
    /// Idempotent against an already-finalized execution: the DB write
    /// returns `None` for a non-live row, which maps to `AlreadyTerminal`.
    pub(super) async fn finalize_driver_terminal_error(
        &self,
        execution: &crate::work::WorkExecution,
        detail: &str,
    ) -> StopOutcome {
        let completion = match self.work_db.fail_pane_parked_execution(&execution.id, detail) {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    ?err,
                    "driver terminal error: failed to record execution failure",
                );
                return StopOutcome::DbError;
            }
        };
        // The run is definitively over — drop every in-flight cache so
        // nothing lingers for this now-terminal execution.
        self.staged_pr_urls.forget(&execution.id);
        self.nudge_breaker.forget(&execution.id);
        self.build_wait_tracker.forget(&execution.id);
        self.background_children_tracker.forget(&execution.id);
        self.hold_registry.release(&execution.id);
        crate::structured_output::clear_all(&self.structured_output_dir, &execution.id);
        // Reap termination path: tear down any driver-owned state outside the
        // workspace (e.g. Codex's per-run CODEX_HOME) before the lease itself
        // is released.
        crate::driver_teardown::teardown_driver_workspace(
            &self.work_db,
            &execution.id,
            execution.workspace_path.as_deref().map(std::path::Path::new),
        )
        .await;
        if let Some(lease_id) = execution.cube_lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await
        {
            tracing::error!(
                execution_id = %execution.id,
                lease_id,
                ?err,
                "driver terminal error: cube workspace release failed",
            );
        }
        self.pane_releaser.release_pane(&execution.id).await;

        let body = format!(
            "The worker's own driver reported its terminal turn boundary as an unrecoverable \
             error, so the engine failed this execution instead of treating a dead process as a \
             clean completion or nudging it for a result it can never produce.\n\n\
             **Driver-reported error:**\n\n{detail}\n\n\
             The execution's cube lease and worker slot have been released."
        );
        if let Err(err) = self
            .file_execution_attention(
                execution,
                DRIVER_TERMINAL_ERROR_ATTENTION_KIND,
                "Worker failed: driver reported an unrecoverable error",
                body,
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "driver terminal error: failed to file attention item",
            );
        }

        self.publisher
            .publish(
                &completion.id,
                &execution.work_item_id,
                completion.status.as_str(),
                "worker_driver_terminal_error",
            )
            .await;
        tracing::error!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            kind = %execution.kind,
            detail,
            "driver terminal error: execution failed (driver reported an unrecoverable error)",
        );
        StopOutcome::DriverTerminalError {
            detail: detail.to_owned(),
        }
    }

    /// File a human-visible attention item recording that a reviewer worker
    /// exhausted its re-prompts without ever producing a readable
    /// `ReviewResult`, so its PR is advancing to Review **unreviewed**. Unlike
    /// [`Self::park_for_unproductive_nudges`], this does NOT change the
    /// execution's terminal handling — the caller still finalises the reviewer
    /// pass and advances the producing task — it only surfaces the give-up to
    /// the human. Best-effort: a filing failure is logged and swallowed.
    pub(super) async fn file_review_result_giveup_attention(
        &self,
        execution: &crate::work::WorkExecution,
        nudge_count: u32,
    ) {
        let body = format!(
            "The automated reviewer for this PR stopped {nudge_count} time(s) without writing a \
             valid ReviewResult — neither the structured-output artifact nor the transcript \
             fallback validated. The producing task is advancing to Review WITHOUT an automated \
             revision; review the PR by hand."
        );
        // Execution-scoped (see `file_execution_attention`): mirrors the
        // nudge-breaker attention so `list_attention_items(&execution.id)`
        // surfaces it.
        if let Err(err) = self
            .file_execution_attention(
                execution,
                REVIEW_RESULT_GIVEUP_ATTENTION_KIND,
                "Reviewer produced no valid ReviewResult",
                body,
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "pr_review finalize: failed to file review-result give-up attention item",
            );
        }
    }
}
