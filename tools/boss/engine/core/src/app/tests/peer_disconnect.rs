//! A peer that closes its socket without reading everything the engine
//! sent ends its session cleanly: no error, and the session is gone from
//! the broker. Linux surfaces that close to the server as `ECONNRESET`
//! rather than EOF, which is exactly how a `boss` CLI invocation that exits
//! on its reply, or the app quitting mid-burst, looks from the engine.

use super::*;
use tokio::net::UnixStream;

#[test]
fn reset_and_abort_are_peer_disconnects_but_other_read_errors_are_not() {
    use std::io::{Error, ErrorKind};
    assert!(read_error_is_peer_disconnect(&Error::from(ErrorKind::ConnectionReset)));
    assert!(read_error_is_peer_disconnect(&Error::from(
        ErrorKind::ConnectionAborted
    )));
    assert!(!read_error_is_peer_disconnect(&Error::from(ErrorKind::UnexpectedEof)));
    assert!(!read_error_is_peer_disconnect(&Error::from(ErrorKind::InvalidData)));
    assert!(!read_error_is_peer_disconnect(&Error::other("boom")));
}

/// Wait until the session's writer has made more socket progress than
/// `after`, i.e. the frame it was sending has reached the peer's buffer.
async fn wait_for_write_progress(
    sink: &SessionSink,
    after: Option<tokio::time::Instant>,
) -> Option<tokio::time::Instant> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let progress = sink.queue.lock().unwrap().last_write_progress;
            if progress > after {
                break progress;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the writer never flushed the frame")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_closing_with_unread_frames_ends_the_session_cleanly() {
    let (state, _dir) = test_server_state();
    let (client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(handle_frontend_connection(server, state.clone(), None));
    let (session, sink) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some((id, sink)) = state.topic_broker.inner.lock().await.sinks.iter().next() {
                break (id.clone(), sink.clone());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The Hello frame, then one push, both flushed into the client's receive
    // buffer while the client reads nothing at all.
    let hello = wait_for_write_progress(&sink, None).await;
    state
        .topic_broker
        .subscribe(&session, &["unread.topic".to_owned()])
        .await;
    state
        .topic_broker
        .publish(
            "unread.topic",
            FrontendEventEnvelope::push(FrontendEvent::TopicEvent {
                topic: "unread.topic".to_owned(),
                revision: 1,
                origin_session_id: String::new(),
                origin_request_id: None,
                event: TopicEventPayload::ResyncRequired,
            }),
        )
        .await;
    wait_for_write_progress(&sink, hello).await;

    drop(client);

    tokio::time::timeout(Duration::from_secs(5), handler)
        .await
        .expect("session did not end after the peer closed")
        .unwrap()
        .expect("a peer closing with unread frames is a clean end of session, not an error");
    assert!(
        !state.topic_broker.inner.lock().await.sinks.contains_key(&session),
        "teardown must unregister the session however the read loop ended"
    );
    assert!(sink.is_closed());
}
