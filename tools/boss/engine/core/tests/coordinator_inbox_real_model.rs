//! Opt-in real-model handoff check using a private config, tmux server and local write sink.
#[path = "coordinator_inbox_live.rs"]
mod live;

#[tokio::test]
async fn real_model_obeys_peer_attributed_reset_request() -> anyhow::Result<()> {
    live::real_model_handoff().await
}
