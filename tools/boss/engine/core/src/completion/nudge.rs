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
    /// 2026-07-02, exec_18b5243e65ff188_2d).
    pub(super) async fn nudge_or_park(
        &self,
        execution: &crate::work::WorkExecution,
        probe_text: &str,
        fingerprint: &str,
        bound_pr_url: Option<&str>,
        proceed_outcome: StopOutcome,
    ) -> StopOutcome {
        // Operator hold (`bossctl agents hold`): the most explicit
        // suppression signal there is — a human deliberately exempted this
        // run from the idle-park sweep. Checked before every other signal
        // so a held run never even reaches the breaker, matching
        // `crate::hold_registry`'s "sweeps must respect it" contract.
        if let Some(record) = self.hold_registry.get(&execution.id) {
            tracing::info!(
                execution_id = %execution.id,
                reason = record.reason.as_deref().unwrap_or("(none given)"),
                "auto-nudge: suppressed — execution is held by an operator",
            );
            self.publisher
                .publish(
                    &execution.id,
                    &execution.work_item_id,
                    execution.status.as_str(),
                    "worker_held",
                )
                .await;
            return StopOutcome::Held { reason: record.reason };
        }
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
        // Build-wait suppression (2026-07-14 log-volume incident): a
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
        // Background-children suppression (observed live 2026-07-17): the
        // worker's own turn genuinely ended (Stop fired, hooks go quiet), but its
        // process tree still has live descendants outside the foreground
        // driver process group — a backgrounded subagent spawned via the
        // harness Agent tool that
        // has not yet reported back with a task-notification. That is
        // WAITING, not stalled: nudging it just manufactures the next
        // Stop and burns the breaker cap below, exactly like the
        // build-wait case above. Checked before `nudge_breaker` for the
        // same reason — a suppressed Stop must never burn the cap.
        // Bounded by `background_children_tracker`'s horizon so a
        // genuinely wedged subagent (one that never exits) still
        // eventually reaches the normal nudge/park path.
        // NOTE: none of the arms below clear the tracked *intent* on their
        // own anymore — only the wait-duration horizon. Whether the intent
        // itself survives this call is decided once, after the breaker
        // verdict is known, below. Clearing it here (the previous shape)
        // meant `NudgeDecision::TooSoon` always found the intent already
        // gone, so `pending_background_nudge_execution_ids()` silently
        // dropped the execution on the very first debounced recheck —
        // exactly the stranding this suppression exists to prevent.
        let delegated_descendant_count = self
            .background_activity_probe
            .live_delegated_descendant_count(&execution.id);
        match delegated_descendant_count {
            Ok(0) => {
                // A previously delegated child exited between recurring sweeps.
                // Clear its wait baseline before the normal nudge proceeds.
                self.background_children_tracker.forget_horizon(&execution.id);
            }
            Ok(descendant_count) => {
                let now_epoch_secs = boss_engine_utils::epoch_time::now_epoch_secs();
                match self.background_children_tracker.record(
                    &execution.id,
                    now_epoch_secs,
                    self.background_children_horizon_secs,
                ) {
                    BuildWaitDecision::Suppress { waited_secs } => {
                        self.background_children_tracker.record_intent(
                            &execution.id,
                            BackgroundNudgeIntent {
                                probe_text: probe_text.to_owned(),
                                fingerprint: fingerprint.to_owned(),
                                bound_pr_url: bound_pr_url.map(str::to_owned),
                                proceed_outcome: proceed_outcome.clone(),
                                activity_watermark: self.background_activity_probe.activity_watermark(&execution.id),
                                hold: NudgeHold::BackgroundChildren,
                            },
                        );
                        tracing::info!(
                            execution_id = %execution.id,
                            descendant_count,
                            waited_secs,
                            horizon_secs = self.background_children_horizon_secs,
                            "auto-nudge: suppressed — worker has live background children; holding \
                             (breaker not consulted, no probe queued)"
                        );
                        self.publisher
                            .publish(
                                &execution.id,
                                &execution.work_item_id,
                                execution.status.as_str(),
                                "worker_background_children_pending",
                            )
                            .await;
                        return StopOutcome::BackgroundChildrenPending {
                            descendant_count,
                            waited_secs,
                        };
                    }
                    BuildWaitDecision::Expired { waited_secs } => {
                        self.background_children_tracker.forget_horizon(&execution.id);
                        tracing::warn!(
                            execution_id = %execution.id,
                            descendant_count,
                            waited_secs,
                            horizon_secs = self.background_children_horizon_secs,
                            "auto-nudge: background-children horizon elapsed — no longer suppressing, \
                             falling back to the normal nudge/park flow"
                        );
                        // Fall through to the normal nudge/park flow below.
                    }
                }
            }
            Err(reason) => {
                self.background_children_tracker.forget_horizon(&execution.id);
                tracing::debug!(
                    execution_id = %execution.id,
                    %reason,
                    "auto-nudge: background-child probe indeterminate — proceeding to nudge; \
                     suppression precondition could not be evaluated"
                );
            }
        }
        let outcome = match self.nudge_breaker.record(
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
                proceed_outcome.clone()
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
                StopOutcome::NudgeDebounced
            }
            NudgeDecision::Trip { count } => {
                self.park_for_unproductive_nudges(execution, count, bound_pr_url, "no new commit, PR, or state change")
                    .await
            }
        };
        // A debounced Stop carries no evidence the worker resumed — the
        // nudge simply didn't fire this round because of the fingerprint
        // debounce window — so the intent must stay observable for the
        // recurring background-nudge recheck sweep
        // (`pending_background_nudge_execution_ids`) to re-evaluate later.
        // Re-record with fresh context (a new watermark, in particular) so
        // the recheck compares against current state, not stale state from
        // whenever this intent was first captured. Every other outcome
        // means the nudge/park path actually concluded (or an early
        // `BackgroundChildrenPending`/similar suppression above already
        // returned), so any tracked intent is stale and safe to drop.
        //
        // Tagged `Debounced`, and that tag is load-bearing: this record is
        // the ONLY thing that can ever move this execution again. Its
        // worker's turn just ended, and the sole producer of the "next
        // Stop" this arm is waiting for is a probe — which is exactly what
        // the debounce declined to send. Retiring it on hook activity (the
        // `BackgroundChildren` rule) leaves nothing scheduled to look at the
        // execution again; see [`NudgeHold::Debounced`].
        if matches!(outcome, StopOutcome::NudgeDebounced) {
            self.background_children_tracker.record_intent(
                &execution.id,
                BackgroundNudgeIntent {
                    probe_text: probe_text.to_owned(),
                    fingerprint: fingerprint.to_owned(),
                    bound_pr_url: bound_pr_url.map(str::to_owned),
                    proceed_outcome,
                    activity_watermark: self.background_activity_probe.activity_watermark(&execution.id),
                    hold: NudgeHold::Debounced,
                },
            );
        } else {
            self.background_children_tracker.forget_intent(&execution.id);
        }
        outcome
    }

    /// Whether `outcome` means [`Self::nudge_or_park`] actually put a probe
    /// on the run's FIFO. Every suppression/park/debounce outcome returns
    /// `false`; anything else is the caller's `proceed_outcome`, which is
    /// only ever returned from the `NudgeDecision::Proceed` arm — the one
    /// arm that calls `queue_probe`.
    ///
    /// Spelled as an exhaustive match rather than a negated list so a new
    /// suppression outcome added later fails to compile here instead of
    /// silently being treated as "a probe was queued" and triggering a
    /// delivery for a probe that does not exist.
    fn nudge_queued_a_probe(outcome: &StopOutcome) -> bool {
        !matches!(
            outcome,
            StopOutcome::NudgeDebounced
                | StopOutcome::NudgeBreakerParked { .. }
                | StopOutcome::BackgroundChildrenPending { .. }
                | StopOutcome::BuildWaitPending { .. }
                | StopOutcome::EscalationPending { .. }
                | StopOutcome::Held { .. }
        )
    }

    /// Execution ids whose Stop-boundary nudge is currently suppressed by
    /// delegated background work. The merge poller includes these in every
    /// recurring pass so the horizon is enforced without another Stop.
    pub(crate) fn pending_background_nudge_execution_ids(&self) -> Vec<String> {
        self.background_children_tracker.execution_ids()
    }

    /// Re-evaluate one previously suppressed nudge from the recurring merge
    /// poller. Replays the exact probe/fingerprint/bound-PR context captured at
    /// Stop, so revision nudges remain revision nudges and no-PR nudges remain
    /// no-PR nudges. Returns `None` when the execution has moved on.
    pub(crate) async fn recheck_background_nudge(&self, execution_id: &str) -> Option<StopOutcome> {
        let intent = self.background_children_tracker.intent(execution_id)?;
        // Did the worker run a tool since this intent was recorded? Only a
        // POSITIVE answer counts: both the recorded and the current
        // watermark must be present and differ. A current watermark of
        // `None` is "no hook-only evidence available right now" (e.g. the
        // registry entry momentarily turned terminal or was re-registered) —
        // not proof the worker resumed — so it must never on its own be read
        // as activity. Treating a `None` as activity is the fail-closed bug
        // this check exists to avoid: it would strand a worker that never
        // resumed by discarding the one record that lets it be rechecked
        // again.
        let current_watermark = self.background_activity_probe.activity_watermark(execution_id);
        let worker_ran_a_tool_since_hold = match (intent.activity_watermark.as_deref(), current_watermark.as_deref()) {
            (Some(recorded), Some(current)) => recorded != current,
            _ => false,
        };
        if worker_ran_a_tool_since_hold {
            match intent.hold {
                // The suppression was a bet that the worker would resume on
                // its own once its delegated children reported back. It did.
                // The bet paid off, so the held nudge is moot: retire it.
                NudgeHold::BackgroundChildren => {
                    tracing::debug!(
                        execution_id,
                        "auto-nudge: recurring background-child recheck retired after worker hook activity resumed"
                    );
                    self.background_children_tracker.forget(execution_id);
                    return None;
                }
                // A debounce is NOT a bet on resumption — it is a pacing
                // delay on a decision already taken at a real turn boundary.
                // Tool activity means the worker is doing something right
                // now, which is a good reason to hold the nudge one more
                // interval rather than talk over live work, but it is not a
                // reason to destroy the record: if the worker goes quiet
                // again (the exact stall this exists to break) the intent is
                // the only thing left that can re-drive it. So re-record
                // against the CURRENT watermark and defer — activity defers,
                // quiescence advances.
                NudgeHold::Debounced => {
                    tracing::debug!(
                        execution_id,
                        "auto-nudge: debounced nudge deferred another interval — worker ran a tool \
                         since the hold; re-recording the intent against the new watermark rather \
                         than retiring it",
                    );
                    self.background_children_tracker.record_intent(
                        execution_id,
                        BackgroundNudgeIntent {
                            activity_watermark: current_watermark,
                            ..intent
                        },
                    );
                    return Some(StopOutcome::NudgeDebounced);
                }
            }
        }
        // Accept any live post-Stop status. Pane-spawned workers park in
        // `Running` (`RunWaitState::WorkerPaneAlive`), not `WaitingHuman`;
        // restricting recheck to `WaitingHuman` alone dropped every intent
        // on the first recurring sweep (CI: `engine_lib_test` shards 1/9).
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) if execution.status.is_live() => execution,
            Ok(_) => {
                self.background_children_tracker.forget(execution_id);
                return None;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "auto-nudge: recurring background-child recheck could not load execution; \
                     will retry on the next sweep"
                );
                return None;
            }
        };

        // Consult the probe and horizon directly first. When the outcome is
        // unchanged from the last pass (still delegated descendants, still
        // inside the horizon), return the same `BackgroundChildrenPending`
        // without re-reading the transcript or re-publishing the
        // invalidation event — a long-running delegated subagent would
        // otherwise produce a steady stream of duplicate publishes and log
        // lines every sweep for the entire horizon. Only fall through to
        // the full `nudge_or_park` path (which does that IO) once the
        // outcome actually might have changed: the child exited or the
        // horizon elapsed.
        if let Ok(descendant_count) = self
            .background_activity_probe
            .live_delegated_descendant_count(execution_id)
            && descendant_count > 0
            && let BuildWaitDecision::Suppress { waited_secs } = self.background_children_tracker.record(
                execution_id,
                boss_engine_utils::epoch_time::now_epoch_secs(),
                self.background_children_horizon_secs,
            )
        {
            tracing::debug!(
                execution_id,
                descendant_count,
                waited_secs,
                "auto-nudge: recurring background-child recheck unchanged — still suppressed, \
                 no republish"
            );
            return Some(StopOutcome::BackgroundChildrenPending {
                descendant_count,
                waited_secs,
            });
        }

        let hold = intent.hold;
        // A `Debounced` hold re-driven while the worker is mid-turn (e.g. a
        // single long tool call such as `bazel test`) must not advance the
        // ladder. `activity_watermark` only moves on PostToolUse, so it is
        // frozen for the whole in-flight call — without this guard the
        // retention path would treat that freeze as quiescence, advance the
        // breaker on every sweep, and eventually park/terminalize a worker
        // that is still genuinely working. Same signal the staged-PR reap
        // path uses; returns false when no registry is wired, so unit tests
        // that do not model live activity are unaffected.
        if hold == NudgeHold::Debounced && self.observed_mid_turn(execution_id) {
            tracing::debug!(
                execution_id,
                "auto-nudge: debounced nudge deferred another interval — worker is mid-turn \
                 (in-flight tool); re-recording the intent rather than advancing the ladder",
            );
            self.background_children_tracker.record_intent(
                execution_id,
                BackgroundNudgeIntent {
                    activity_watermark: current_watermark,
                    ..intent
                },
            );
            return Some(StopOutcome::NudgeDebounced);
        }
        let replay = intent.clone();
        let outcome = self
            .nudge_or_park(
                &execution,
                &intent.probe_text,
                &intent.fingerprint,
                intent.bound_pr_url.as_deref(),
                intent.proceed_outcome,
            )
            .await;
        if !matches!(
            outcome,
            StopOutcome::BackgroundChildrenPending { .. }
                | StopOutcome::BuildWaitPending { .. }
                | StopOutcome::EscalationPending { .. }
                | StopOutcome::Held { .. }
                | StopOutcome::NudgeDebounced
        ) {
            self.background_children_tracker.forget_intent(execution_id);
        }
        // A `Debounced` hold re-driven from here must survive its own
        // successful nudge. `nudge_or_park` drops the intent whenever the
        // nudge actually fires, which is correct on the Stop path — the
        // worker was handed a probe on a live boundary and its reply
        // produces the next Stop. It is wrong here: this execution reached
        // this path precisely because it emits no further Stops, so dropping
        // the record after one sweep-driven nudge puts the ladder straight
        // back into the absorbing state, one rung higher. Re-record so each
        // subsequent pass can advance it, all the way to the breaker's own
        // terminal.
        //
        // Gated on `nudge_queued_a_probe`, not merely "not parked":
        // suppression outcomes (BackgroundChildrenPending, BuildWaitPending,
        // …) may themselves have recorded a different hold tag, and
        // re-writing `..replay` (which still carries `Debounced`) would
        // clobber that tag. `NudgeDebounced` is already re-recorded by
        // `nudge_or_park` itself. Only a probe that actually advanced the
        // ladder needs the retention re-record.
        if hold == NudgeHold::Debounced && Self::nudge_queued_a_probe(&outcome) {
            self.background_children_tracker.record_intent(
                execution_id,
                BackgroundNudgeIntent {
                    activity_watermark: self.background_activity_probe.activity_watermark(execution_id),
                    ..replay
                },
            );
        }
        if Self::nudge_queued_a_probe(&outcome) {
            // A probe queued from here has no Stop fan-out to ride out on:
            // `dispatch_probe_on_stop` / `dispatch_probe_on_post_tool_use`
            // only run on a worker hook event, and this whole path exists
            // precisely because no further hook is coming. Without an
            // explicit delivery the probe sits in the FIFO forever while the
            // breaker counts it as an unproductive nudge the worker was
            // never actually shown.
            self.probe_queuer.deliver_queued_probes_now(execution_id);
            if hold == NudgeHold::Debounced {
                NUDGE_LADDER_SWEEP_ADVANCED.inc(&self.metrics);
                tracing::warn!(
                    execution_id,
                    work_item_id = %execution.work_item_id,
                    kind = %execution.kind,
                    fingerprint = %intent.fingerprint,
                    "auto-nudge: recurring sweep advanced a DEBOUNCED nudge ladder — the worker \
                     went quiet at a boundary whose decision was withheld for pacing, so no Stop \
                     was ever going to advance it. Out-of-band delivery to the pane has been \
                     requested; the bounded nudge/park ladder now proceeds to its terminal.",
                );
            }
        }
        Some(outcome)
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
        // Legibility (2026-07-14 log-volume incident): the parked/yellow
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
        // pile up identical items. Deduping *stamps* the existing row's
        // `last_raised_at` rather than merely observing it: the kind is
        // `ClearedBy::WorkResumed`, and a breaker that trips again while its
        // first row is still open must not then be resolved by a run that
        // started before the current trip.
        let already_filed = self
            .work_db
            .reraise_open_execution_attention(&execution.id, NUDGE_BREAKER_ATTENTION_KIND)
            .unwrap_or(None)
            .is_some();
        if !already_filed
            && let Err(err) = self
                .file_execution_attention(
                    execution,
                    NUDGE_BREAKER_ATTENTION_KIND,
                    "Worker parked: auto-nudge loop bounded",
                    reason.clone(),
                )
                .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "nudge breaker: failed to file attention item; parking without UI surface"
            );
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
        // Captured before `record_worker_idle_abandonment` below nulls
        // `workspace_path` in the same transaction that terminalizes the
        // execution — this path terminalizes a parked-live execution, so it
        // owns driver teardown.
        let workspace_path = execution.workspace_path.clone();
        // Marked before the terminalizing write — see `super::teardown`.
        let teardown = self.begin_teardown(&execution.id);
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
        self.background_children_tracker.forget(&execution.id);
        self.hold_registry.release(&execution.id);
        self.finish_worker_teardown(
            &execution.id,
            completion.released_lease_id.as_deref(),
            workspace_path.as_deref().map(std::path::Path::new),
            "idle_park",
            teardown,
        )
        .await;
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
