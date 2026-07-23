use super::server::current_parent_pid;
use super::*;
use crate::protocol::TopicEventPayload;

fn topic_envelope(topic: &str, revision: u64) -> FrontendEventEnvelope {
    FrontendEventEnvelope::push_with_revision(
        revision,
        FrontendEvent::TopicEvent {
            topic: topic.to_owned(),
            revision,
            origin_session_id: "test".to_owned(),
            origin_request_id: None,
            event: TopicEventPayload::WorkInvalidated {
                reason: "test".to_owned(),
                product_id: None,
                item_ids: vec![],
            },
        },
    )
}

fn response_envelope(request_id: &str) -> FrontendEventEnvelope {
    FrontendEventEnvelope::response(request_id.to_owned(), FrontendEvent::ProductsList { products: vec![] })
}

/// A priority-lane envelope: the small engine→app control push
/// (`EngineRequest`) that `send_to_app` issues and blocks on. Used to
/// exercise the priority lane independently of the bulk lane.
fn engine_request_envelope(request_id: &str) -> FrontendEventEnvelope {
    FrontendEventEnvelope::push(FrontendEvent::EngineRequest {
        request_id: request_id.to_owned(),
        request: EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput {
            slot_id: 1,
            kill_grace_seconds: 5,
        }),
    })
}

fn topic_of(env: &FrontendEventEnvelope) -> Option<String> {
    topic_event_topic(&env.payload)
}

/// Build a `ServerState` backed by a throwaway on-disk DB. The returned
/// `TempDir` must be kept alive for as long as the `ServerState` is used —
/// dropping it deletes the backing `state.db`.
pub(super) fn test_server_state() -> (Arc<ServerState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .build(),
        None,
    ));
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(cfg, None, None, None, None, None, None).unwrap();
    (state, temp)
}

pub(super) fn make_session_sink() -> Arc<SessionSink> {
    let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
    Arc::new(SessionSink::new(shutdown_tx))
}

mod app_channel;
mod context;
mod engine_health_report;
mod proposals;
mod session_sink_queue;
mod t02;
mod t03;
mod t04;
mod t05;
mod t06;
mod t07;
mod trust_authorization;
mod worker_pane_interaction;
mod worker_pane_lifecycle;
mod worker_probe_dispatch;
mod worker_process_reaping;
mod worker_tier;
