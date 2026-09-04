//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct PaneParkedFailure<'a> {
    teardown_reason: &'static str,
    attention_kind: &'a str,
    attention_title: &'a str,
    attention_body: String,
    publish_event: &'static str,
    log_label: &'a str,
    outcome: StopOutcome,
}

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
        // Marked before the terminalizing write so no sweep can observe
        // this execution terminal-with-a-live-pane without also seeing
        // that its own teardown owns the pane — see `super::teardown`.
        let teardown = self.begin_teardown(&execution.id);
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
        self.finish_worker_teardown(
            &execution.id,
            &completion.execution.work_item_id,
            completion.released_lease_id.as_deref(),
            workspace_path.as_deref().map(std::path::Path::new),
            "no_op",
            teardown,
        )
        .await;
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

    /// File the human-visible record that a `revision_implementation`
    /// closed on the sanctioned `NO_CHANGES_NEEDED` marker without ever
    /// moving the bound PR — i.e. the worker declared the review finding
    /// it was dispatched for needs no code change.
    ///
    /// Always filed, never conditional: this terminal is the one path on
    /// which a revision completes successfully with the finding
    /// unaddressed, so the human who asked for it has to be able to see
    /// that it was declined rather than fixed. Best-effort — a filing
    /// failure is logged and swallowed, exactly like the other attention
    /// helpers here; it must never block the completion itself.
    pub(super) async fn file_revision_no_op_attention(
        &self,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
    ) {
        let body = format!(
            "This revision worker pushed no commits — the bound PR's head SHA is unchanged from \
             the last known baseline (the dispatch-time snapshot, or a later baseline absorbed \
             when a concurrently-active parent worker's push was observed) — and ended by emitting \
             the sanctioned `NO_CHANGES_NEEDED` marker, its explicit claim that the review finding \
             needs no code change.\n\n\
             The revision has been closed as a declared no-op: no PR was opened, nothing was \
             pushed, and the bound PR ({bound_pr_url}) is untouched. **The finding that produced \
             this revision was therefore never addressed.** Read the worker's final message to \
             judge whether declining it was right; re-dispatch the revision if it was not.\n\n\
             The execution's cube lease and worker slot have been released."
        );
        if let Err(err) = self
            .file_execution_attention(
                execution,
                REVISION_NO_OP_ATTENTION_KIND,
                "Revision closed without addressing its finding",
                body,
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "revision no-op: failed to file attention item; closing without a UI surface",
            );
        }
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
        let body = format!(
            "The worker's own driver reported its terminal turn boundary as an unrecoverable \
             error, so the engine failed this execution instead of treating a dead process as a \
             clean completion or nudging it for a result it can never produce.\n\n\
             **Driver-reported error:**\n\n{detail}\n\n\
             The execution's cube lease and worker slot have been released."
        );
        self.finalize_pane_parked_failure(
            execution,
            detail,
            PaneParkedFailure::builder()
                .teardown_reason("driver_terminal_error")
                .attention_kind(DRIVER_TERMINAL_ERROR_ATTENTION_KIND)
                .attention_title("Worker failed: driver reported an unrecoverable error")
                .attention_body(body)
                .publish_event("worker_driver_terminal_error")
                .log_label("driver terminal error")
                .outcome(StopOutcome::DriverTerminalError {
                    detail: detail.to_owned(),
                })
                .build(),
        )
        .await
    }

    /// Finalize a completed remote worker whose structured-output artifact
    /// the engine could not pull off its host — [`crate::host_adapter::CollectOutcome::Failed`]:
    /// positive proof (an invalid descriptor, or a copy that ran and failed)
    /// that the result sitting on the remote host cannot be trusted here.
    /// The worker itself may have finished cleanly; this is an engine-side
    /// retrieval failure, not a driver-reported error, so it gets its own
    /// attention kind/title/body rather than borrowing
    /// [`Self::finalize_driver_terminal_error`]'s language, which would tell
    /// the UI a false causal story ("the driver failed") for a cause that
    /// was actually "the engine couldn't copy the file".
    pub(super) async fn finalize_remote_collection_failure(
        &self,
        execution: &crate::work::WorkExecution,
        host_id: &str,
        reason: &str,
    ) -> StopOutcome {
        let detail = format!("remote structured-output collection failed on host {host_id}: {reason}");
        let body = format!(
            "The worker on remote host `{host_id}` reached its terminal turn boundary, but the \
             engine could not retrieve its structured-output result from that host — the driver \
             itself reported nothing wrong.\n\n\
             **Collection failure:**\n\n{reason}\n\n\
             The execution's cube lease and worker slot have been released."
        );
        self.finalize_pane_parked_failure(
            execution,
            &detail,
            PaneParkedFailure::builder()
                .teardown_reason("remote_collection_failed")
                .attention_kind(REMOTE_COLLECTION_FAILED_ATTENTION_KIND)
                .attention_title("Worker failed: remote result could not be collected")
                .attention_body(body)
                .publish_event("worker_remote_collection_failed")
                .log_label("remote collection failure")
                .outcome(StopOutcome::RemoteCollectionFailed { detail: detail.clone() })
                .build(),
        )
        .await
    }

    /// Shared teardown + fail-and-file-attention body for an execution the
    /// engine must terminalize for a reason that is not the worker's own
    /// driver reporting an error — parameterized by attention kind/title/body
    /// (and the teardown/publish event labels) so each failure class tells
    /// its own true story instead of all borrowing driver-terminal-error's.
    async fn finalize_pane_parked_failure(
        &self,
        execution: &crate::work::WorkExecution,
        detail: &str,
        failure: PaneParkedFailure<'_>,
    ) -> StopOutcome {
        // Marked before the terminalizing write — see `super::teardown`.
        let teardown = self.begin_teardown(&execution.id);
        let completion = match self.work_db.fail_pane_parked_execution(&execution.id, detail) {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    ?err,
                    "{}: failed to record execution failure", failure.log_label,
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
        // The driver reported its process already dead, but "already dead"
        // is the driver's claim, not the engine's observation — the pane is
        // still mapped until `release_pane` reaps it. So this path takes the
        // same ordering as every other: pane, then driver state, then lease.
        // Previously it tore driver state down FIRST, which is exactly the
        // window in which a Codex process the driver mis-reported as dead
        // could refresh its auth token after the credential file had been
        // adopted back.
        self.finish_worker_teardown(
            &execution.id,
            &execution.work_item_id,
            execution.cube_lease_id.as_deref(),
            execution.workspace_path.as_deref().map(std::path::Path::new),
            failure.teardown_reason,
            teardown,
        )
        .await;

        if let Err(err) = self
            .file_execution_attention(
                execution,
                failure.attention_kind,
                failure.attention_title,
                failure.attention_body,
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "{}: failed to file attention item", failure.log_label,
            );
        }

        self.publisher
            .publish(
                &completion.id,
                &execution.work_item_id,
                completion.status.as_str(),
                failure.publish_event,
            )
            .await;
        tracing::error!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            kind = %execution.kind,
            detail,
            "{}: execution failed", failure.log_label,
        );
        failure.outcome
    }

    /// File a human-visible attention item recording that a reviewer worker
    /// exhausted its re-prompts without ever producing a readable
    /// `ReviewResult`, so its task stays in Doing pending recovery. Unlike
    /// [`Self::park_for_unproductive_nudges`], this does NOT change the
    /// execution's terminal handling — the caller still finalises the reviewer
    /// pass — it only surfaces the give-up to the human. Best-effort: a filing
    /// failure is logged and swallowed.
    pub(super) async fn file_review_result_giveup_attention(
        &self,
        execution: &crate::work::WorkExecution,
        nudge_count: u32,
    ) {
        let body = format!(
            "The automated reviewer for this PR stopped {nudge_count} time(s) without writing a \
             valid ReviewResult — neither the structured-output artifact nor the transcript \
             fallback validated. The producing task remains in Doing until a replacement review \
             records a result."
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
