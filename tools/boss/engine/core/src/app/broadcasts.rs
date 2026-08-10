//! Topic broadcasts pushed to subscribed frontends.
//!
//! Each helper snapshots a piece of engine state and publishes it on its
//! topic so subscribers re-render without polling. Called from wherever the
//! underlying state changes (the events-socket consumer, the spawn flow, the
//! GitHub auth forwarder, the health-affecting RPC handlers).
//!
//! Split out of `app.rs`; pure structural move — no behavioural change.

use super::*;

impl ServerState {
    /// Snapshot of every allocated worker slot's live runtime state.
    pub fn live_worker_states_snapshot(&self) -> Vec<crate::protocol::LiveWorkerState> {
        self.live_worker_states.snapshot()
    }

    /// Push the current live-worker-state snapshot on the
    /// `worker.live_states` topic. Called whenever the events-socket
    /// consumer or the spawn flow mutates the registry.
    pub async fn broadcast_live_worker_states(&self) {
        let states = self.live_worker_states.snapshot();
        let envelope = FrontendEventEnvelope::push(FrontendEvent::WorkerLiveStatesList { states });
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
    /// crate::coordinator::ExecutionCoordinator::subscribe_pause_state) —
    /// the *state change* — rather than being invoked by each pauser:
    /// before this existed the push lived inside the two `Set*Paused` RPC
    /// handlers, so `bossctl dispatch pause` updated the running app while
    /// the spawn-capability circuit breaker's programmatic pause left it
    /// showing nothing, and the operator had to go to the CLI to discover
    /// dispatch had been held for hours. Any future programmatic pauser is
    /// covered here for free; none of them needs to know this exists.
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
}
