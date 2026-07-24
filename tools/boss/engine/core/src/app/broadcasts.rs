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

    /// Push a Boothby pass lifecycle change on the `boothby.activity` topic.
    /// Called by [`crate::boothby_scheduler::spawn_loop`] via the
    /// [`crate::boothby_scheduler::BoothbyActivitySink`] impl below.
    pub async fn broadcast_boothby_activity(&self, pass: boss_protocol::BoothbyPass) {
        let envelope = FrontendEventEnvelope::push(FrontendEvent::BoothbyActivity { pass });
        self.topic_broker.publish(TOPIC_BOOTHBY_ACTIVITY, envelope).await;
    }
}
