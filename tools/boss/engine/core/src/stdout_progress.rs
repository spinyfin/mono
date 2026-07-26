//! Engine attachment point for the stdout-JSONL progress ingress.
//!
//! [`crate::events_socket`] is the ingress for a
//! [`crate::driver::ProgressIngress::HookCallback`] driver: its worker's hooks
//! fire the `boss-event` shim, which forwards each payload over the unix
//! events socket. A [`crate::driver::ProgressIngress::StdoutJsonl`] driver has
//! no shim and no hooks — its worker writes one JSON envelope per line to its
//! own stdout and nothing forwards it. Without something reading that stream
//! such a worker emits no progress at all: it never leaves `Spawning`, the
//! staleness sweep sees no cadence, and no `Stop` ever reaches the completion
//! handler.
//!
//! This module is the engine side of that reader. The framing, parsing, and
//! tolerance machinery lives in [`boss_engine_stdout_progress`]; here we
//! resolve the run's driver through [`crate::driver::DriverRegistry`], adapt
//! each decoded envelope into the same [`IncomingHookEvent`] the socket path
//! produces, and hand it to a [`WorkerEventSink`]. The engine's sink is
//! `ServerState`, whose implementation calls the identical dispatch fan-out
//! the socket accept loop calls — so both transports drive one activity
//! machine over one code path, and the stdout arm is an additional ingress
//! rather than a parallel implementation.
//!
//! **Claude is unaffected.** `ClaudeDriver` is a `HookCallback` driver; its
//! progress keeps arriving over the events socket exactly as before and never
//! touches this module.
//!
//! # What is not here
//!
//! Nothing in the engine currently spawns a worker whose stdout it owns —
//! workers run in libghostty panes hosted by the app, and the only registered
//! driver is Claude. [`run_stdout_progress_ingress`] is therefore generic over
//! [`AsyncRead`]: a child process's `ChildStdout`, an SSH channel, or a test
//! pipe all attach unchanged once a `StdoutJsonl` driver and a spawn path that
//! can hand over a stream exist.

use boss_engine_stdout_progress::{ReaderStats, StdoutJsonlProgressReader};
use tokio::io::AsyncRead;

use crate::events_socket::IncomingHookEvent;

/// Where decoded progress events go.
///
/// A seam rather than a direct `ServerState` call so the ingress loop below
/// depends on nothing but the event shape: the engine implements this to run
/// its dispatch fan-out, and tests implement it to record what arrived.
#[async_trait::async_trait]
pub trait WorkerEventSink: Send + Sync {
    /// Handle one decoded progress event. Called once per envelope, in stream
    /// order; the reader is paced by how long this takes, so an implementation
    /// that blocks blocks the worker's stdout pipe.
    async fn dispatch_worker_event(&self, incoming: IncomingHookEvent);
}

/// A driver slug that is not in the [`crate::driver::DriverRegistry`].
///
/// Slugs are validated at write time, but a version skew or a stale DB row can
/// still produce one this binary does not know. Surfaced as an error rather
/// than an unwrap so a bad slug fails the one run instead of the engine.
#[derive(Debug, thiserror::Error)]
#[error("unknown driver slug: {0}")]
pub struct UnknownDriverError(pub String);

/// Read `stream` as `run_id`'s stdout-JSONL progress stream until it ends,
/// dispatching every envelope the run's driver recognises to `sink`.
///
/// `driver_slug` is resolved through [`crate::driver::DriverRegistry`] — the
/// normalisation applied to the stream is a property of the run's driver, not
/// of this call site, which is what lets a second `StdoutJsonl` backend land
/// without touching this function.
///
/// Returns the reader's [`ReaderStats`] so the caller can log what the stream
/// actually produced. A worker that ends its stream having emitted zero events
/// is the diagnostic case those counters exist for: `lines_read` tells "silent
/// worker" apart from "driver could not decode anything it said".
///
/// Never panics on stream content and never returns early on a bad line — see
/// the [`boss_engine_stdout_progress`] crate docs for the tolerated-anomaly
/// list. The loop ends only at end of stream.
pub async fn run_stdout_progress_ingress<R, S>(
    run_id: &str,
    driver_slug: &str,
    stream: R,
    sink: &S,
) -> Result<ReaderStats, UnknownDriverError>
where
    R: AsyncRead + Unpin,
    S: WorkerEventSink + ?Sized,
{
    let driver = crate::driver::DriverRegistry::default()
        .get(driver_slug)
        .ok_or_else(|| UnknownDriverError(driver_slug.to_owned()))?
        .clone();
    tracing::info!(run_id, driver = driver_slug, "stdout progress: ingress started");

    let mut reader = StdoutJsonlProgressReader::new(stream, driver);
    while let Some(envelope) = reader.next_event().await {
        // `peer_pid` is `None` by construction: it exists so the socket
        // ingress can attribute an anonymous connection to a process. Here the
        // caller owns the process being read, so `run_id` is known outright
        // and there is nothing to attribute.
        sink.dispatch_worker_event(IncomingHookEvent {
            peer_pid: None,
            run_id: Some(run_id.to_owned()),
            transcript_path: envelope.transcript_path,
            event: envelope.event,
        })
        .await;
    }

    let stats = reader.stats();
    tracing::info!(
        run_id,
        driver = driver_slug,
        lines_read = stats.lines_read,
        events_emitted = stats.events_emitted,
        blank_lines = stats.blank_lines,
        non_json_lines = stats.non_json_lines,
        unrecognised_envelopes = stats.unrecognised_envelopes,
        oversized_lines = stats.oversized_lines,
        unterminated_tails = stats.unterminated_tails,
        ended_with_io_error = stats.ended_with_io_error,
        "stdout progress: ingress ended",
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::protocol::WorkerActivity;

    /// Claude-shaped payloads, because `ClaudeDriver` is the only registered
    /// driver today. What varies here is the *transport* — these arrive as
    /// stdout lines rather than over the socket — and the assertions are that
    /// the activity machine reaches the same states either way, with an
    /// unknown variant and a non-JSON line tolerated along the way.
    const CLAUDE_SHAPED_STDOUT: &str = concat!(
        r#"{"hook_event_name":"SessionStart","session_id":"s","source":"startup"}"#,
        "\n",
        "some interleaved non-JSON worker chatter\n",
        r#"{"hook_event_name":"PreToolUse","session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
        "\n",
        r#"{"hook_event_name":"SomeFutureHook","session_id":"s"}"#,
        "\n",
        r#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false}"#,
        "\n",
    );

    /// Sink that drives the real [`LiveWorkerStateRegistry`] — the same
    /// activity machine `dispatch_live_worker_state` drives from the hook
    /// path — and records the activity after each event.
    struct ActivitySink {
        registry: LiveWorkerStateRegistry,
        slot_id: u8,
        seen: Mutex<Vec<WorkerActivity>>,
        run_ids: Mutex<Vec<Option<String>>>,
    }

    impl ActivitySink {
        fn new() -> Self {
            let registry = LiveWorkerStateRegistry::new();
            let slot_id = 0;
            registry.register_spawn(slot_id, "exec-1", "model", 1_000, None);
            Self {
                registry,
                slot_id,
                seen: Mutex::new(Vec::new()),
                run_ids: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkerEventSink for ActivitySink {
        async fn dispatch_worker_event(&self, incoming: IncomingHookEvent) {
            self.run_ids.lock().unwrap().push(incoming.run_id.clone());
            self.registry.apply_event(self.slot_id, &incoming.event);
            let activity = self.registry.get(self.slot_id).unwrap().activity;
            self.seen.lock().unwrap().push(activity);
        }
    }

    /// The load-bearing claim: envelopes read off stdout move the *existing*
    /// activity machine exactly as hook events do — spawning → working → idle.
    #[tokio::test]
    async fn stdout_envelopes_drive_the_activity_machine() {
        let sink = ActivitySink::new();
        assert_eq!(
            sink.registry.get(sink.slot_id).unwrap().activity,
            WorkerActivity::Spawning,
        );

        let stats = run_stdout_progress_ingress(
            "exec-1",
            crate::effort::ENGINE_DEFAULT_DRIVER,
            CLAUDE_SHAPED_STDOUT.as_bytes(),
            &sink,
        )
        .await
        .expect("the engine default driver is always registered");

        assert_eq!(
            *sink.seen.lock().unwrap(),
            vec![
                WorkerActivity::Idle,    // SessionStart(startup) leaves Spawning
                WorkerActivity::Working, // PreToolUse
                WorkerActivity::Idle,    // Stop
            ],
        );
        assert_eq!(stats.events_emitted, 3);
        assert_eq!(stats.non_json_lines, 1);
        assert_eq!(stats.unrecognised_envelopes, 1);
    }

    /// The stdout stream carries no run-correlation field (the socket path's
    /// `_boss_run_id` has no analog), so the ingress stamps the run id it was
    /// opened for onto every event. Without it every event would be dropped by
    /// the dispatcher's missing-run_id guard.
    #[tokio::test]
    async fn every_event_carries_the_run_id_the_stream_was_opened_for() {
        let sink = ActivitySink::new();
        run_stdout_progress_ingress(
            "exec-42",
            crate::effort::ENGINE_DEFAULT_DRIVER,
            CLAUDE_SHAPED_STDOUT.as_bytes(),
            &sink,
        )
        .await
        .unwrap();

        let run_ids = sink.run_ids.lock().unwrap();
        assert_eq!(run_ids.len(), 3);
        assert!(run_ids.iter().all(|id| id.as_deref() == Some("exec-42")));
    }

    /// The transcript path is resolved through the driver's
    /// `TranscriptAccess` capability, exactly as the socket ingress does, so
    /// live-status transcript discovery works on either transport.
    #[tokio::test]
    async fn transcript_path_rides_through_from_the_payload() {
        struct Capture(Mutex<Vec<Option<String>>>);

        #[async_trait::async_trait]
        impl WorkerEventSink for Capture {
            async fn dispatch_worker_event(&self, incoming: IncomingHookEvent) {
                self.0.lock().unwrap().push(incoming.transcript_path);
            }
        }

        let stream = concat!(
            r#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"transcript_path":"/t/s.jsonl"}"#,
            "\n",
            r#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false}"#,
            "\n",
        );
        let sink = Capture(Mutex::new(Vec::new()));
        run_stdout_progress_ingress("exec-1", crate::effort::ENGINE_DEFAULT_DRIVER, stream.as_bytes(), &sink)
            .await
            .unwrap();

        assert_eq!(*sink.0.lock().unwrap(), vec![Some("/t/s.jsonl".to_owned()), None],);
    }

    /// An unrecognised slug must fail the run, not panic the engine — and must
    /// fail before any of the stream is consumed.
    #[tokio::test]
    async fn unknown_driver_slug_is_an_error_not_a_panic() {
        let sink = ActivitySink::new();
        let err = run_stdout_progress_ingress("exec-1", "codex", CLAUDE_SHAPED_STDOUT.as_bytes(), &sink)
            .await
            .expect_err("no 'codex' driver is registered yet");
        assert_eq!(err.0, "codex");
        assert!(sink.seen.lock().unwrap().is_empty());
    }

    /// A worker that dies before writing anything must end the ingress cleanly
    /// with an all-zero tally rather than hanging or erroring.
    #[tokio::test]
    async fn an_empty_stream_ends_cleanly() {
        let sink = ActivitySink::new();
        let stats = run_stdout_progress_ingress("exec-1", crate::effort::ENGINE_DEFAULT_DRIVER, &b""[..], &sink)
            .await
            .unwrap();

        assert_eq!(stats, ReaderStats::default());
        assert!(sink.seen.lock().unwrap().is_empty());
    }

    /// A stream of nothing but envelopes this driver cannot decode must still
    /// terminate cleanly, and must leave behind the counters that tell a
    /// driver/stream mismatch apart from a silent worker. This is the shape a
    /// real Codex stream has under `ClaudeDriver`: well-formed JSONL that the
    /// wrong normaliser rejects wholesale.
    #[tokio::test]
    async fn a_wholly_unrecognised_stream_is_visible_in_the_stats() {
        let stream = concat!(
            r#"{"type":"thread.started","thread_id":"019f974c-3d59-7533-b320-3963123c809b"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":25699}}"#,
            "\n",
        );
        let sink = ActivitySink::new();
        let stats =
            run_stdout_progress_ingress("exec-1", crate::effort::ENGINE_DEFAULT_DRIVER, stream.as_bytes(), &sink)
                .await
                .unwrap();

        assert_eq!(stats.lines_read, 3);
        assert_eq!(stats.events_emitted, 0);
        assert_eq!(stats.unrecognised_envelopes, 3);
        assert!(sink.seen.lock().unwrap().is_empty());
        assert_eq!(
            sink.registry.get(sink.slot_id).unwrap().activity,
            WorkerActivity::Spawning,
            "an undecodable stream must leave the activity machine untouched, not corrupt it",
        );
    }
}
