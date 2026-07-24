//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Generic auto-nudge gate. Records the intent to nudge `execution`
    /// against the circuit breaker (keyed by `fingerprint`, which must
    /// encode the work state so an unchanged state counts as
    /// unproductive) and either:
    ///
    /// - queues `probe_text`, publishes the awaiting-PR signal, and
    ///   returns `proceed_outcome` (the nudge fired); or
    /// - parks the execution via [`Self::park_for_unproductive_nudges`]
    ///   and returns [`StopOutcome::NudgeBreakerParked`] (the breaker
    ///   tripped — `max_unproductive_nudges` consecutive nudges fired
    ///   with no state change).
    ///
    /// This is the single choke point for the nudge loop: bounding it
    /// here makes the breaker generic to *every* auto-nudge, not just
    /// the "produce a PR" one. It is also the suppression point for
    /// worker-declared escalations/blockers (below): before touching the
    /// circuit breaker at all, refuse to nudge an execution that has an
    /// unresolved `[effort-escalation]`/`[blocked]` attention item —
    /// dogging a worker that just told the coordinator it's stuck awaiting
    /// direction is exactly the failure this exists to prevent (incident
    /// 2026-07-02, exec_18b5243e65ff188_2d / T2085).
    pub(super) async fn nudge_or_park(
        &self,
        execution: &crate::work::WorkExecution,
        probe_text: &str,
        fingerprint: &str,
        bound_pr_url: Option<&str>,
        proceed_outcome: StopOutcome,
    ) -> StopOutcome {
        if let Some(reason) = self.unresolved_worker_signal_reason(execution) {
            tracing::info!(
                execution_id = %execution.id,
                %reason,
                "auto-nudge: suppressed — worker has an unresolved escalation/blocker awaiting \
                 coordinator action",
            );
            self.publisher
                .publish(
                    &execution.id,
                    &execution.work_item_id,
                    execution.status.as_str(),
                    "worker_escalation_pending",
                )
                .await;
            return StopOutcome::EscalationPending { reason };
        }
        // Build-wait suppression (2026-07-14 incident, T2608 / T2612): a
        // worker narrating that it is legitimately waiting on a
        // backgrounded build/test gate must not be nudged — each nudge
        // wakes it, gets an unproductive-but-honest "still building,
        // waiting" reply, and manufactures the very Stop cadence that
        // exhausts the breaker below. Check this BEFORE touching
        // `nudge_breaker` so a suppressed Stop never burns the cap; once
        // the worker's own armed monitor wakes it with real news (a push,
        // a different reply), the next Stop simply won't match the
        // heuristic and falls through to the normal flow. Bounded by
        // `build_wait_tracker`'s horizon so a worker that keeps saying
        // "waiting" forever without ever finishing still eventually
        // reaches the normal nudge/park path (requirement: genuine wedge
        // detection must keep working).
        if let Some(text) = self.read_final_triage_message(&execution.id).await.into_message()
            && let Some(signal) = detect_build_wait_signal(&text)
        {
            let now_epoch_secs = boss_engine_utils::epoch_time::now_epoch_secs();
            match self
                .build_wait_tracker
                .record(&execution.id, now_epoch_secs, self.build_wait_horizon_secs)
            {
                BuildWaitDecision::Suppress { waited_secs } => {
                    tracing::info!(
                        execution_id = %execution.id,
                        matched_phrase = signal.matched_phrase,
                        waited_secs,
                        horizon_secs = self.build_wait_horizon_secs,
                        "auto-nudge: suppressed — worker is narrating a legitimate backgrounded \
                         build/test wait (breaker not consulted, no probe queued)"
                    );
                    self.publisher
                        .publish(
                            &execution.id,
                            &execution.work_item_id,
                            execution.status.as_str(),
                            "worker_build_wait_pending",
                        )
                        .await;
                    return StopOutcome::BuildWaitPending { waited_secs };
                }
                BuildWaitDecision::Expired { waited_secs } => {
                    tracing::warn!(
                        execution_id = %execution.id,
                        matched_phrase = signal.matched_phrase,
                        waited_secs,
                        horizon_secs = self.build_wait_horizon_secs,
                        "auto-nudge: build-wait horizon elapsed — no longer suppressing, falling \
                         back to the normal nudge/park flow"
                    );
                    // Fall through to the normal nudge/park flow below.
                }
            }
        }
        match self.nudge_breaker.record(
            &execution.id,
            fingerprint,
            self.max_unproductive_nudges,
            (self.now_fn)(),
        ) {
            NudgeDecision::Proceed { count } => {
                tracing::info!(
                    execution_id = %execution.id,
                    nudge_count = count,
                    max = self.max_unproductive_nudges,
                    "auto-nudge: queueing probe (under circuit-breaker cap)"
                );
                self.publish_awaiting_pr(execution).await;
                self.probe_queuer.queue_probe(&execution.id, probe_text);
                proceed_outcome
            }
            NudgeDecision::TooSoon { since_last } => {
                // The identical fingerprint was just nudged; a Stop this
                // close on its heels can't carry new information (see
                // `nudge_breaker` module docs — this is the fix for the
                // 2026-07-14 exec_18c21b03972f3920_49 incident: three
                // identical "push to the existing PR" probes fired 8-9s
                // apart against a revision that had already pushed).
                // Wait quietly rather than re-sending the same probe text;
                // the next Stop (or the merge poller) re-evaluates from
                // scratch and can still finalize or nudge once state
                // actually moves.
                tracing::debug!(
                    execution_id = %execution.id,
                    fingerprint,
                    since_last_ms = since_last.as_millis(),
                    "auto-nudge: suppressed — identical fingerprint re-fired inside the debounce \
                     window; waiting for external state to change before re-nudging",
                );
                proceed_outcome
            }
            NudgeDecision::Trip { count } => {
                self.park_for_unproductive_nudges(execution, count, bound_pr_url, "no new commit, PR, or state change")
                    .await
            }
        }
    }

    /// Park `execution` because the auto-nudge circuit breaker tripped
    /// (or because nudging it is structurally wrong, e.g. a
    /// `ci_remediation` exec with no bound PR). Files a (deduplicated)
    /// attention item with a human-readable reason and publishes
    /// `AttentionItemCreated` so the coordinator/UI surfaces it, then
    /// publishes a distinct live-state reason. The execution stays in
    /// `waiting_human` — that *is* the parked-for-human state — but the
    /// engine stops nudging it.
    pub(super) async fn park_for_unproductive_nudges(
        &self,
        execution: &crate::work::WorkExecution,
        nudge_count: u32,
        bound_pr_url: Option<&str>,
        detail: &str,
    ) -> StopOutcome {
        let pr_clause = match bound_pr_url {
            Some(url) => format!("A PR already exists for this work: {url}."),
            None => "No PR was produced.".to_owned(),
        };
        // Legibility (2026-07-14 incident, T2608 / T2612): the parked/yellow
        // state — active, no live execution, autostart cleared — carries no
        // surfaced reason of its own; an operator staring at the row has no
        // way to tell it apart from an ordinary backlog item without opening
        // this attention item. Stamp the explicit wall-clock time the park
        // happened so at least "why is this yellow, and since when" is
        // answerable at a glance.
        let parked_at =
            boss_engine_utils::iso8601::format_epoch_iso8601(boss_engine_utils::epoch_time::now_epoch_secs());
        let reason = if nudge_count > 0 {
            format!(
                "Auto-nudge circuit breaker tripped: nudged {nudge_count} times with {detail}. \
{pr_clause} Parked for human review at {parked_at}. The execution's cube lease and worker slot \
have been released; `autostart` has been cleared so the automated rescan will not immediately \
re-dispatch a replacement worker onto this task/chore — status is otherwise left unchanged for \
re-dispatch or manual review."
            )
        } else {
            format!(
                "Worker parked without nudging: {detail}. {pr_clause} Parked at {parked_at}. The \
execution's cube lease and worker slot have been released; `autostart` has been cleared so the \
automated rescan will not immediately re-dispatch a replacement worker onto this task/chore — \
status is otherwise left unchanged for re-dispatch or manual review."
            )
        };

        // Deduplicate: only one open attention item of this kind per
        // execution, so repeated Stops after the breaker trips don't
        // pile up identical items.
        let already_filed = self
            .work_db
            .list_attention_items(&execution.id)
            .map(|items| {
                items
                    .iter()
                    .any(|i| i.kind == NUDGE_BREAKER_ATTENTION_KIND && i.status != "resolved")
            })
            .unwrap_or(false);
        if !already_filed {
            match self.work_db.create_attention_item(CreateAttentionItemInput {
                execution_id: Some(execution.id.clone()),
                work_item_id: None,
                kind: NUDGE_BREAKER_ATTENTION_KIND.to_owned(),
                status: None,
                title: "Worker parked: auto-nudge loop bounded".to_owned(),
                body_markdown: reason.clone(),
                resolved_at: None,
            }) {
                Ok(item) => {
                    if let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id) {
                        let product_id = work_item.product_id().to_string();
                        self.publisher
                            .publish_frontend_event_on_product(
                                &product_id,
                                FrontendEvent::AttentionItemCreated { item },
                            )
                            .await;
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        execution_id = %execution.id,
                        ?err,
                        "nudge breaker: failed to file attention item; parking without UI surface"
                    );
                }
            }
        }

        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                execution.status.as_str(),
                "worker_nudge_breaker_parked",
            )
            .await;
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            kind = %execution.kind,
            nudge_count,
            %reason,
            "auto-nudge circuit breaker tripped — parked execution, no further nudges"
        );
        // Release the slot/lease this execution would otherwise hold
        // forever — the `exec_18b932df99d17658_475` incident this closes:
        // a worker concluded there was nothing left to do, the breaker
        // parked it, and it sat holding its cube lease and worker pane
        // indefinitely until an operator noticed and reaped it by hand.
        // The attention item filed above is the durable human-facing
        // surface; this is what actually frees the resources.
        self.finalize_idle_park(execution, &reason).await;
        StopOutcome::NudgeBreakerParked { reason }
    }

    /// Finalize an execution the auto-nudge circuit breaker gave up on:
    /// release its cube lease and worker pane so it stops holding a slot
    /// forever. Mirrors [`Self::finalize_no_op_completion`]'s teardown
    /// mechanics, but deliberately does NOT touch the task/chore status —
    /// there is no positive evidence the work is done here, only that
    /// further automated nudging is unproductive (see
    /// [`crate::work::WorkDb::record_worker_idle_abandonment`] for why that
    /// distinction matters, including why it clears `autostart` to stop an
    /// automated abandon/re-dispatch churn loop). The attention item
    /// [`Self::park_for_unproductive_nudges`] already filed is the durable
    /// surface for a human to review or re-dispatch the work item.
    ///
    /// Best-effort and idempotent: a DB write against an already-terminal
    /// execution is a silent no-op (the row was already finalized by a
    /// concurrent path), and a lease-release failure is logged, never
    /// propagated — this must never block the Stop-boundary response. The
    /// lease/pane release also proceeds even if the task/chore row itself
    /// was hard-deleted out from under the execution — `record_worker_idle_abandonment`
    /// returns `work_item: None` in that case rather than erroring the
    /// whole finalize, so the work-item-changed publish is simply skipped.
    pub(super) async fn finalize_idle_park(&self, execution: &crate::work::WorkExecution, detail: &str) {
        let completion = match self.work_db.record_worker_idle_abandonment(&execution.id, detail) {
            Ok(Some(completion)) => completion,
            Ok(None) => return,
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    ?err,
                    "idle-park finalize: failed to record",
                );
                return;
            }
        };
        self.staged_pr_urls.forget(&execution.id);
        self.nudge_breaker.forget(&execution.id);
        self.build_wait_tracker.forget(&execution.id);
        if let Some(lease_id) = completion.released_lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await
        {
            tracing::error!(
                execution_id = %execution.id,
                lease_id,
                ?err,
                "idle-park finalize: cube release failed"
            );
        }
        self.pane_releaser.release_pane(&execution.id).await;
        let work_item_id = completion.execution.work_item_id.clone();
        self.publisher
            .publish(
                &completion.execution.id,
                &work_item_id,
                completion.execution.status.as_str(),
                "worker_idle_park_finalized",
            )
            .await;
        match completion.work_item.as_ref() {
            Some(work_item) => {
                let product_id = work_item.product_id().to_string();
                self.publisher
                    .publish_work_item_changed(&product_id, &work_item_id, "worker_idle_park_finalized")
                    .await;
            }
            None => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %work_item_id,
                    "idle-park finalize: task/chore row missing, skipping work-item-changed publish",
                );
            }
        }
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %work_item_id,
            "idle-park finalize: cube lease and worker slot released; execution abandoned; \
             autostart cleared so the automated rescan won't immediately re-dispatch it",
        );
    }
}
