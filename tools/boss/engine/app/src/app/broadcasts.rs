//! Topic broadcasts pushed to subscribed frontends.
//!
//! Each helper snapshots a piece of engine state and publishes it on its
//! topic so subscribers re-render without polling. Called from wherever the
//! underlying state changes (the events-socket consumer, the spawn flow, the
//! GitHub auth forwarder, the health-affecting RPC handlers).
//!
//! Split out of `app.rs`; pure structural move — no behavioural change.

use super::*;
use boss_engine_storage_values::attention_kinds::HUNG_WORKER_ATTENTION_KIND;

impl ServerState {
    /// Reconcile warning transitions without changing execution or tool state.
    pub(super) fn refresh_hung_workers(
        &self,
        previous: &mut std::collections::HashMap<String, (String, Option<String>)>,
    ) -> bool {
        let current: std::collections::HashMap<_, _> = self
            .refresh_background_task_verdicts()
            .into_iter()
            .filter_map(|state| Some((state.run_id, (state.work_item_id?, state.hung_since))))
            .collect();
        let mut changed = *previous != current;
        let mut persisted = true;
        // Persisted warnings outlive this cache. Protect executions still awaiting
        // adoption, then reconcile warnings even when both snapshots are empty.
        let open = self
            .work_db
            .list_open_attention_items_of_kind(HUNG_WORKER_ATTENTION_KIND);
        let adopting = self.work_db.list_in_flight_executions();
        let (open, adopting) = match (open, adopting) {
            (Ok(open), Ok(adopting)) => (open, adopting),
            (Err(error), _) | (_, Err(error)) => {
                tracing::warn!(?error, "failed to load persisted hung warnings");
                return false;
            }
        };
        let mut hung = std::collections::HashMap::new();
        for (run, (item, since)) in &current {
            if let Some(since) = since {
                let owner = self.work_db.owning_work_item_id(item);
                // Stable selection avoids rewriting an owner's warning as siblings
                // change iteration order.
                let candidate = (since, run);
                let selected = hung.entry(owner).or_insert(candidate);
                if candidate < *selected {
                    *selected = candidate;
                }
            }
        }
        for attention in &open {
            let Some(owner) = &attention.work_item_id else { continue };
            let pending = adopting.iter().any(|execution| {
                !current.contains_key(&execution.id)
                    && self.work_db.owning_work_item_id(&execution.work_item_id) == *owner
            });
            if !hung.contains_key(owner) && !pending {
                match self
                    .work_db
                    .resolve_external_tracker_attention(owner, HUNG_WORKER_ATTENTION_KIND)
                {
                    Ok(_) => changed = true,
                    Err(error) => {
                        persisted = false;
                        tracing::warn!(?error, "failed to resolve hung warning");
                    }
                }
            }
        }
        for (owner, (since, run)) in hung {
            // Dismissal lasts for the owner's continuous hung episode.
            // An empty restart cache still reconciles persisted warnings.
            if previous
                .values()
                .any(|(item, since)| since.is_some() && self.work_db.owning_work_item_id(item) == owner)
            {
                continue;
            }
            let body = format!(
                "Driver has made no observable progress since {since}, while awaiting model output with no blocking tool poll or declared wait. This is a suspected hang, not a confirmed failure: provider retries can recover. Inspect the worker and interrupt or restart it if needed. Boss has not killed it. Execution: `{run}`."
            );
            match self.work_db.upsert_external_tracker_attention(
                &owner,
                HUNG_WORKER_ATTENTION_KIND,
                "Worker hung; inspect or restart",
                &body,
            ) {
                Ok(_) => changed = true,
                Err(error) => {
                    persisted = false;
                    tracing::warn!(?error, "failed to raise hung warning");
                }
            }
        }
        if persisted {
            *previous = current;
        }
        changed
    }

    /// Snapshot of every allocated worker slot's live runtime state.
    ///
    /// Performs no database, filesystem or process probing: the `agy`
    /// background-task verdict that suppresses hang detection is the one the
    /// periodic [`Self::refresh_hung_workers`] tick last computed off the async
    /// runtime, so the hook-driven broadcasts stay lightweight.
    pub fn live_worker_states_snapshot(&self) -> Vec<boss_protocol::LiveWorkerState> {
        let live_background = self.agy_background_tasks.live_runs();
        self.build_live_worker_states(|run_id| live_background.contains(run_id))
    }

    /// Build the snapshot, asking `has_live_background_task(run_id)`
    /// whether each run is covered by a still-running `agy` background task.
    fn build_live_worker_states(
        &self,
        mut has_live_background_task: impl FnMut(&str) -> bool,
    ) -> Vec<boss_protocol::LiveWorkerState> {
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let mut states = self.live_worker_states.snapshot();
        for state in &mut states {
            let in_turn = self
                .live_worker_states
                .semantic_progress_for_slot(state.slot_id)
                .is_some_and(|progress| {
                    progress.turn_condition
                        == boss_engine_live_worker_state::semantic_progress::SemanticTurnCondition::InTurn
                });
            if state.activity == boss_protocol::WorkerActivity::WaitOverdue {
                if let Some(wait) = self.wait_registry.lapsed_record(&state.run_id, now) {
                    state.wait_reason = Some(wait.reason);
                    state.wait_expires_at =
                        Some(boss_engine_utils::iso8601::format_epoch_iso8601(wait.expires_at_epoch));
                }
            } else if let Some(wait) = self.wait_registry.active(&state.run_id, now)
                && !in_turn
            {
                state.wait_reason = Some(wait.reason);
                state.wait_expires_at = Some(boss_engine_utils::iso8601::format_epoch_iso8601(wait.expires_at_epoch));
                // An unexpired declared wait means the worker is waiting on a
                // job, not on the owner. The hook-derived `WaitingForInput`
                // activity (and its "awaiting input" subtitle) would make the
                // owner look needed, so report WaitingOnWork consistently with the registry.
                // A prompt raised mid-turn, before the turn that declared the
                // wait ended, is still a real ask of the owner.
                let turn_ended = self
                    .live_worker_states
                    .last_stop_ingress_seq(state.slot_id)
                    .is_some_and(|stop| wait.declared_after_seq.is_none_or(|declared| stop > declared));
                if turn_ended
                    && state.activity == boss_protocol::WorkerActivity::WaitingForInput
                    && !self.live_worker_states.has_blocking_prompt(state.slot_id)
                {
                    state.activity = boss_protocol::WorkerActivity::WaitingOnWork;
                    if state.live_status.as_deref() == Some(boss_engine_runtime::live_status::AWAITING_INPUT_LITERAL) {
                        state.live_status = Some(boss_engine_runtime::live_status::WAITING_ON_WORK_LITERAL.into());
                    }
                }
            }
        }
        for state in &mut states {
            let checkpoint = self.live_worker_states.semantic_progress_for_slot(state.slot_id);
            let fidelity = self.live_worker_states.progress_fidelity_for_slot(state.slot_id);
            let held = self.hold_registry.is_held(&state.run_id);
            let has_active_wait = self.wait_registry.active(&state.run_id, now).is_some();
            let tool_completed = self.live_worker_states.tool_completed_since_prompt(state.slot_id);
            let mut signals = boss_engine_runtime::hung_worker::HungSignals {
                held,
                has_active_wait,
                has_blocking_tool_poll: false,
                tool_completed_since_prompt: tool_completed,
            };
            signals.has_blocking_tool_poll = has_live_background_task(&state.run_id);
            state.hung_since =
                boss_engine_runtime::hung_worker::hung_since(state, checkpoint.as_ref(), fidelity, signals, now);
        }
        states
    }

    /// Recompute which runs have a live `agy` background task and publish the
    /// verdicts for [`Self::live_worker_states_snapshot`]. Blocking: it reads
    /// SQLite and transcript files, so callers run it off the async runtime.
    fn refresh_background_task_verdicts(&self) -> Vec<boss_protocol::LiveWorkerState> {
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let mut live = std::collections::HashSet::new();
        let states = self.build_live_worker_states(|run_id| {
            let verdict = self.agy_has_live_background_task(run_id, now);
            if verdict {
                live.insert(run_id.to_owned());
            }
            verdict
        });
        self.agy_background_tasks.replace_live_runs(live);
        states
    }

    /// Whether `execution_id` is an `agy` run whose transcript shows a
    /// background task that is not a daemon, has no matching completion
    /// `SYSTEM_MESSAGE`, and was launched within the unmatched-`RUNNING`
    /// horizon, while the worker process has not been seen dead.
    fn agy_has_live_background_task(&self, execution_id: &str, now: i64) -> bool {
        use boss_engine_completion::agy_background_task::BackgroundTaskLiveness;

        let Some(driver) =
            boss_engine_completion::driver_transcript::driver_for_spawned_execution(&self.work_db, execution_id)
        else {
            return false;
        };
        if driver.descriptor().name != "agy" {
            return false;
        }
        let transcript_path = self
            .transcript_path_cache
            .get(execution_id)
            .or_else(|| self.work_db.transcript_path_for_execution(execution_id).ok().flatten());
        let Some(transcript_path) = transcript_path else {
            return false;
        };
        let containment_root = match driver.transcript_containment_root(execution_id) {
            Ok(root) => root,
            Err(error) => {
                if self.agy_background_tasks.first_warning(execution_id) {
                    tracing::warn!(execution_id, %error, "agy transcript containment check failed");
                }
                return false;
            }
        };
        self.agy_background_tasks.poll_with_probe(
            execution_id,
            std::path::Path::new(&transcript_path),
            containment_root.as_deref(),
            now,
            || match self.live_worker_states.shell_pid_for_run(execution_id) {
                Some(pid) if pid > 0 && super::process_signals::pid_is_alive(pid as libc::pid_t) => {
                    BackgroundTaskLiveness::Alive
                }
                // Only a dead worker is positive evidence the task is gone. A
                // live process whose task is quiet stays covered until the
                // per-task horizon expires.
                Some(pid) if pid > 0 => BackgroundTaskLiveness::Exited,
                _ => BackgroundTaskLiveness::Unknown,
            },
        )
    }

    /// Push the current live-worker-state snapshot on the
    /// `worker.live_states` topic. Called whenever the events-socket
    /// consumer or the spawn flow mutates the registry.
    pub async fn broadcast_live_worker_states(&self) {
        let envelope = FrontendEventEnvelope::push(self.live_worker_board_snapshot());
        self.topic_broker.publish(TOPIC_WORKER_LIVE_STATES, envelope).await;
    }

    /// Push the current GitHub OAuth auth state on the `github.auth` topic.
    /// Called by the auth forwarder on every state transition so subscribed
    /// frontends re-render the issue-sync "GitHub account" section as the
    /// device flow advances. The DTO is display-safe — the token and the
    /// private device code never appear in it.
    pub async fn broadcast_github_auth_state(&self, state: GitHubAuthStateDto) {
        let envelope = FrontendEventEnvelope::push(FrontendEvent::GitHubAuthState { state });
        self.topic_broker.publish(TOPIC_GITHUB_AUTH, envelope).await;
    }

    /// Push the current engine-health snapshot on the `engine.health` topic.
    /// Called whenever health-affecting state changes (dispatch pause/resume,
    /// etc.) so subscribed frontends update the health banner without polling
    /// or restarting.
    pub async fn broadcast_engine_health(self: &Arc<Self>) {
        let report = build_engine_health_report(self);
        let envelope = FrontendEventEnvelope::push(FrontendEvent::EngineHealthResult { report });
        self.topic_broker.publish(TOPIC_ENGINE_HEALTH, envelope).await;
    }

    /// Push a fresh engine-health snapshot on every dispatch- or
    /// automation-pause transition, whatever caused it.
    ///
    /// This is the single seam that keeps the app's pause banner honest.
    /// It deliberately subscribes to
    /// [`ExecutionCoordinator::subscribe_pause_state`](
    /// boss_engine_dispatch::coordinator::ExecutionCoordinator::subscribe_pause_state) —
    /// the pause-state *transition* — rather than being called by each
    /// pauser: every pauser, the `Set*Paused` RPC handlers, the
    /// spawn-capability circuit breaker, and any future programmatic one,
    /// pushes a fresh health snapshot through here without knowing this
    /// exists. A pause that only reached the CLI would leave the app's
    /// banner silently stale.
    ///
    /// The app's connect-time `get_engine_health` still covers the
    /// start-into-a-pause case — this task is purely about transitions
    /// observed by an already-running app. Nothing polls: the loop is
    /// parked in `changed()` until a transition happens.
    ///
    /// Supervised, so a panic inside the loop restarts it with a fresh
    /// subscription instead of silently ending pause pushes for the
    /// lifetime of the engine. Each (re)start broadcasts once before
    /// waiting, which is the reconcile pass `spawn_supervised` subscribers
    /// owe: it closes the gap where a transition landed while a prior
    /// attempt was between panic and restart.
    pub fn spawn_pause_state_health_broadcaster(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak_state = Arc::downgrade(self);
        boss_event_bus::spawn_supervised("engine_pause_state_health", move || {
            let weak_state = weak_state.clone();
            async move {
                let Some(state) = weak_state.upgrade() else {
                    return;
                };
                let mut pause_changes = state.execution_coordinator.subscribe_pause_state();
                state.broadcast_engine_health().await;
                drop(state);
                while pause_changes.changed().await.is_ok() {
                    // Re-read authoritative state through a fresh upgrade so
                    // this task never keeps the `ServerState` alive past
                    // engine shutdown.
                    let Some(state) = weak_state.upgrade() else {
                        return;
                    };
                    tracing::debug!(
                        dispatch_paused = state.execution_coordinator.is_dispatch_paused(),
                        automation_paused = state.execution_coordinator.is_automation_paused(),
                        "pause state changed — broadcasting engine health to subscribed frontends",
                    );
                    state.broadcast_engine_health().await;
                    state.republish_queued_dispatch_if_changed().await;
                }
                // The sender lives inside the coordinator, which lives
                // inside `ServerState`; `changed()` therefore only errors
                // once the engine is tearing down. Log it rather than
                // exiting silently so a future refactor that drops the
                // coordinator early is observable instead of quietly
                // ending pause pushes.
                tracing::warn!(
                    "pause-state broadcaster: notifier closed, subscription ended; \
                     pause transitions will no longer push engine health",
                );
            }
        })
    }

    /// Push a fresh engine-health snapshot whenever a pre-start
    /// spawn-failure streak alert is raised, updated or resolved.
    ///
    /// The companion of [`Self::spawn_pause_state_health_broadcaster`] for
    /// [`boss_engine_runtime::pre_start_streak`], built the same way and for the same
    /// reason: the alert is only worth anything if it reaches the app's
    /// banner without a poll or RPC, so the push is keyed to the
    /// tracker's own change notifier rather than to the code that happens
    /// to record a failure. Every in-place update pushes too, so the banner
    /// shows the live count and latest error rather than the values from
    /// the moment the alert was first raised.
    ///
    /// Supervised, and each (re)start broadcasts once before waiting, so a
    /// change that landed across a panic/restart is still delivered.
    pub fn spawn_spawn_streak_health_broadcaster(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak_state = Arc::downgrade(self);
        boss_event_bus::spawn_supervised("engine_spawn_streak_health", move || {
            let weak_state = weak_state.clone();
            async move {
                let Some(state) = weak_state.upgrade() else {
                    return;
                };
                let mut alert_changes = state.execution_coordinator.pre_start_streaks().subscribe();
                state.broadcast_engine_health().await;
                drop(state);
                while alert_changes.changed().await.is_ok() {
                    let Some(state) = weak_state.upgrade() else {
                        return;
                    };
                    tracing::debug!(
                        active_alerts = state.execution_coordinator.pre_start_streaks().active_alerts().len(),
                        "pre-start spawn failure streak changed — broadcasting engine health to \
                         subscribed frontends",
                    );
                    state.broadcast_engine_health().await;
                }
                tracing::warn!(
                    "spawn-streak broadcaster: notifier closed, subscription ended; streak alert \
                     changes will no longer push engine health",
                );
            }
        })
    }

    /// Push a fresh engine-health snapshot whenever the merge poller raises,
    /// updates or withdraws its abandoned-pass notice. Built like
    /// [`Self::spawn_spawn_streak_health_broadcaster`], for the same reason:
    /// the notice exists so a card stuck behind a stalled detection pass is
    /// explained on the banner while it is happening.
    pub fn spawn_merge_detection_stall_health_broadcaster(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak_state = Arc::downgrade(self);
        boss_event_bus::spawn_supervised("engine_merge_detection_stall_health", move || {
            let weak_state = weak_state.clone();
            async move {
                let Some(state) = weak_state.upgrade() else {
                    return;
                };
                let mut stall_changes = state.merge_detection_stall.subscribe();
                state.broadcast_engine_health().await;
                drop(state);
                while stall_changes.changed().await.is_ok() {
                    let Some(state) = weak_state.upgrade() else {
                        return;
                    };
                    tracing::debug!(
                        stalled = stall_changes.borrow().is_some(),
                        "merge detection stall changed — broadcasting engine health to subscribed frontends",
                    );
                    state.broadcast_engine_health().await;
                }
                tracing::warn!(
                    "merge-detection-stall broadcaster: notifier closed, subscription ended; \
                     abandoned detection passes will no longer push engine health",
                );
            }
        })
    }

    /// Push a fresh engine-health snapshot whenever the progression sweep
    /// starts, changes or ends its launch backoff notice, and every minute
    /// while it remains active so relative times stay current. Built like
    /// [`Self::spawn_spawn_streak_health_broadcaster`], for the same reason:
    /// the notice is worth nothing if it only reaches the banner on a poll.
    pub fn spawn_progression_backoff_health_broadcaster(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak_state = Arc::downgrade(self);
        boss_event_bus::spawn_supervised("engine_progression_backoff_health", move || {
            let weak_state = weak_state.clone();
            async move {
                let Some(state) = weak_state.upgrade() else {
                    return;
                };
                let mut backoff_changes = state.progression_launch_backoff.subscribe();
                state.broadcast_engine_health().await;
                drop(state);
                while boss_engine_runtime::progression_sweep::wait_for_backoff_health_refresh(&mut backoff_changes)
                    .await
                    .is_ok()
                {
                    let Some(state) = weak_state.upgrade() else {
                        return;
                    };
                    tracing::debug!(
                        backing_off = backoff_changes.borrow().is_some(),
                        "refreshing progression launch backoff health for subscribed frontends",
                    );
                    state.broadcast_engine_health().await;
                }
            }
        })
    }

    /// Push a Boothby pass lifecycle change on the `boothby.activity` topic.
    /// Called by [`crate::boothby_scheduler::spawn_loop`] via the
    /// [`crate::boothby_scheduler::BoothbyActivitySink`] impl below.
    pub async fn broadcast_boothby_activity(&self, pass: boss_protocol::BoothbyPass) {
        let envelope = FrontendEventEnvelope::push(FrontendEvent::BoothbyActivity { pass });
        self.topic_broker.publish(TOPIC_BOOTHBY_ACTIVITY, envelope).await;
    }
}

#[async_trait::async_trait]
impl boss_engine_runtime::external_tracker::reconcile::WorkInvalidationPublisher for ServerState {
    async fn publish_work_item_invalidated(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.publisher
            .publish_work_item_changed(product_id, work_item_id, reason)
            .await;
    }
}
