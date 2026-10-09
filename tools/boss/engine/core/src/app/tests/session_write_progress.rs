use super::*;
use tokio::io::AsyncReadExt;

fn fill_old_queue(sink: &SessionSink) {
    let mut q = sink.queue.lock().unwrap();
    for i in 0..MAX_SESSION_QUEUE {
        assert_eq!(
            q.enqueue(topic_envelope(&format!("progress.{i}"), 1)),
            EnqueueOutcome::Enqueued
        );
    }
    q.backdate_oldest_bulk_entry(STUCK_CLIENT_AGE_MS + 100);
}

#[tokio::test(start_paused = true)]
async fn progressing_large_frame_survives_old_full_queue() {
    let (tx, mut shutdown) = oneshot::channel();
    let sink = Arc::new(SessionSink::new(tx));
    let broker = TopicBroker::default();
    broker.register_session("progressing-writer", sink.clone()).await;
    let topics: Vec<String> = (0..8).map(|i| format!("updates.{i}")).collect();
    broker.subscribe("progressing-writer", &topics).await;
    fill_old_queue(&sink);

    // A tiny socket buffer forces many partial writes of one large frame.
    // Drain a chunk each second for longer than the stuck threshold.
    let (mut writer, mut reader) = tokio::io::duplex(1024);
    let line = serde_json::to_string(&FrontendEventEnvelope::response(
        "large-response",
        FrontendEvent::Error {
            message: "x".repeat(32 * 1024),
        },
    ))
    .unwrap();
    let expected = format!("{line}\n");
    let writing_sink = sink.clone();
    let writing = tokio::spawn(async move { writing_sink.write_frame(&mut writer, &line, "progressing-writer").await });
    let mut received = Vec::new();
    for topic in topics {
        let mut chunk = [0; 1024];
        let n = reader.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        received.extend_from_slice(&chunk[..n]);
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        sink.queue
            .lock()
            .unwrap()
            .backdate_oldest_bulk_entry(STUCK_CLIENT_AGE_MS + 100);
        broker.publish(&topic, topic_envelope(&topic, 1)).await;
        let q = sink.queue.lock().unwrap();
        assert!(!q.closed && !q.slow);
        assert!(q.last_write_progress.is_some());
        assert_eq!(q.items.len(), MAX_SESSION_QUEUE);
    }
    assert!(!writing.is_finished(), "the large frame is still in flight");
    assert!(matches!(shutdown.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    reader.read_to_end(&mut received).await.unwrap();
    writing.await.unwrap().unwrap();
    assert_eq!(
        received,
        expected.as_bytes(),
        "partial writes preserve framing and all bytes"
    );
}

#[tokio::test(start_paused = true)]
async fn non_reading_client_times_out_without_more_publishes_and_logs_envelopes() {
    let logs = crate::test_support::log_capture::install();
    let (tx, mut shutdown) = oneshot::channel();
    let sink = SessionSink::new(tx);
    fill_old_queue(&sink);
    let (mut writer, _non_reader) = tokio::io::duplex(16);
    let line = serde_json::to_string(&response_envelope("blocked-response")).unwrap();
    let error = sink
        .write_frame(&mut writer, &line, "non-reading-progress-test")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(sink.queue_stats().closed);
    assert_eq!(shutdown.try_recv(), Ok(()));
    let captured = String::from_utf8(logs.lock().clone()).unwrap();
    let warning = captured
        .lines()
        .find(|line| line.contains("non-reading-progress-test") && line.contains("slow subscriber"))
        .unwrap();
    assert!(warning.contains("head_kind=Some(\"topic_event\")"), "{warning}");
    assert!(warning.contains("head_topic=Some(\"progress.0\")"), "{warning}");
    let head_size = serde_json::to_string(&topic_envelope("progress.0", 1)).unwrap().len();
    assert!(
        warning.contains(&format!("head_size_bytes=Some({head_size})")),
        "{warning}"
    );
    assert!(
        warning.contains(&format!("in_flight_size_bytes=Some({})", line.len())),
        "{warning}"
    );
    assert!(warning.contains("in_flight_kind=Some(\"products_list\")"), "{warning}");
}

#[test]
fn old_write_progress_does_not_protect_stuck_client() {
    let (tx, _shutdown) = oneshot::channel();
    let sink = SessionSink::new(tx);
    fill_old_queue(&sink);
    sink.queue.lock().unwrap().last_write_progress =
        Some(tokio::time::Instant::now() - std::time::Duration::from_millis(STUCK_CLIENT_AGE_MS + 100));
    assert_eq!(sink.enqueue(topic_envelope("overflow", 1)), EnqueueOutcome::Slow);
}
