//! Socket progress and stalled-session diagnostics shared with the queue.

use super::*;
use std::io;
use std::time::Duration;
use tokio::io::AsyncWrite;

/// Only routing metadata, never envelope contents, belongs in a kick log.
#[derive(Debug)]
pub(super) struct EnvelopeSummary {
    kind: String,
    topic: Option<String>,
    size_bytes: usize,
}

impl EnvelopeSummary {
    fn from_line(line: &str) -> Self {
        #[derive(serde::Deserialize)]
        struct Envelope {
            payload: Payload,
        }
        #[derive(serde::Deserialize)]
        struct Payload {
            #[serde(rename = "type")]
            kind: String,
            topic: Option<String>,
        }
        // Deserialize only metadata; large payload fields are skipped without
        // allocating another copy of the response tree.
        // Diagnostic parsing must not interrupt delivery of a valid serialized
        // frame (for example, one deeper than the deserializer's depth limit).
        let payload = serde_json::from_str::<Envelope>(line)
            .map(|envelope| envelope.payload)
            .unwrap_or(Payload {
                kind: "unknown".to_owned(),
                topic: None,
            });
        Self {
            kind: payload.kind,
            topic: payload.topic,
            size_bytes: line.len(),
        }
    }
}

impl SessionSink {
    pub(super) fn log_stuck(&self, session_id: &str, reason: &str, topic: Option<&str>) {
        let q = self.queue.lock().expect("session queue lock poisoned");
        let stats = q.stats();
        let queued_head = q.priority.front().or_else(|| q.items.front()).and_then(|(_, env)| {
            serde_json::to_string(env)
                .ok()
                .map(|line| EnvelopeSummary::from_line(&line))
        });
        tracing::warn!(
            session_id,
            reason,
            topic,
            queue_depth = stats.depth,
            priority_depth = stats.priority_depth,
            oldest_age_ms = stats.oldest_age_ms,
            write_idle_ms = ?q.last_write_progress.map(|at| at.elapsed().as_millis()),
            head_kind = ?queued_head.as_ref().map(|head| head.kind.as_str()),
            head_topic = ?queued_head.as_ref().and_then(|head| head.topic.as_deref()),
            head_size_bytes = ?queued_head.as_ref().map(|head| head.size_bytes),
            in_flight_kind = ?q.in_flight.as_ref().map(|head| head.kind.as_str()),
            in_flight_topic = ?q.in_flight.as_ref().and_then(|head| head.topic.as_deref()),
            in_flight_size_bytes = ?q.in_flight.as_ref().map(|head| head.size_bytes),
            "slow subscriber: disconnecting"
        );
    }

    /// A timeout applies to each partial write, not the whole frame. A large
    /// response can take arbitrarily long provided the socket keeps accepting
    /// bytes. A non-reader times out even without further broker publishes.
    pub(super) async fn write_frame<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        line: &str,
        session_id: &str,
    ) -> io::Result<()> {
        self.queue.lock().expect("session queue lock poisoned").in_flight = Some(EnvelopeSummary::from_line(line));
        let result = self.write_frame_bytes(writer, line).await;
        if let Err(err) = &result {
            if err.kind() == io::ErrorKind::TimedOut {
                self.log_stuck(session_id, "socket write made no progress", None);
            } else {
                tracing::error!(session_id, ?err, "failed to write event to frontend socket");
            }
            self.close();
            self.trigger_shutdown();
        }
        self.queue.lock().expect("session queue lock poisoned").in_flight = None;
        result
    }

    async fn write_frame_bytes<W: AsyncWrite + Unpin>(&self, writer: &mut W, line: &str) -> io::Result<()> {
        let idle_timeout = Duration::from_millis(STUCK_CLIENT_AGE_MS);
        for mut bytes in [line.as_bytes(), b"\n"] {
            while !bytes.is_empty() {
                let written = tokio::time::timeout(idle_timeout, writer.write(bytes)).await??;
                if written == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                self.queue
                    .lock()
                    .expect("session queue lock poisoned")
                    .last_write_progress = Some(tokio::time::Instant::now());
                bytes = &bytes[written..];
            }
        }
        tokio::time::timeout(idle_timeout, writer.flush()).await??;
        Ok(())
    }
}
