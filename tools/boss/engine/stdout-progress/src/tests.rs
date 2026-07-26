//! Reader tests.
//!
//! The fixtures are the real `codex exec --json` stdout lines captured in
//! `tools/boss/docs/investigations/codex-progress-channel-decision-2026-07-24.md`,
//! decoded by a Codex-shaped test driver. Using a non-Claude driver here is the
//! point: it proves the reader carries whatever the injected driver recognises
//! rather than assuming Claude's hook payload shape, and it exercises the
//! tolerate-don't-crash paths against a stream that genuinely mixes mappable
//! and unmappable envelopes.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use boss_engine_driver::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, ModelMenu, PermissionArtifacts, PermissionInput,
    ProgressFidelity, ProgressIngress, ProgressObservationConfig, SpawnPlan, SpawnRequest, ToolUseInterceptionConfig,
    ToolUseInterceptionWiring, TurnEnd, WorkerErrorClass,
};
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{EffortLevel, NormalizeError, ReasoningMode, SessionStartSource, WorkerEvent};
use tokio::io::AsyncWriteExt;

use super::*;

// ─── Codex-shaped test driver ───────────────────────────────────────────────

/// Maps `codex exec --json` stdout envelopes onto [`WorkerEvent`]s.
///
/// Deliberately partial — `item.completed` for an `agent_message` has no
/// activity-machine analog and is rejected, exactly as a real driver would
/// reject the variants it has no mapping for. That rejection is what the
/// tolerance tests below lean on.
struct CodexShapedDriver {
    descriptor: DriverDescriptor,
}

// The model menu is irrelevant to progress decoding; these satisfy
// `DriverDescriptor`'s fn-pointer fields with the minimum that compiles.
fn no_effort_value(_level: EffortLevel) -> Option<&'static str> {
    None
}

fn only_model(_level: EffortLevel) -> &'static str {
    "gpt-5.5"
}

fn only_model_for_reasoning(_mode: ReasoningMode) -> &'static str {
    "gpt-5.5"
}

fn no_addendum(_level: EffortLevel) -> Option<&'static str> {
    None
}

fn never_auto_permissions(_model: &str) -> bool {
    false
}

impl CodexShapedDriver {
    fn new() -> Self {
        Self {
            descriptor: DriverDescriptor {
                name: "codex-shaped",
                label: "Codex-shaped test driver",
                binary: "codex",
                config_dir: ".codex",
                agent_rules_filename: "AGENTS.md",
                initial_prompt_filename: "initial-prompt.txt",
                model_menu: ModelMenu {
                    engine_default: "gpt-5.5",
                    effort_value_for_level: no_effort_value,
                    default_model_for_level: only_model,
                    model_for_reasoning: only_model_for_reasoning,
                    prompt_addendum_for_level: no_addendum,
                    model_requires_auto_permissions: never_auto_permissions,
                },
            },
        }
    }

    fn arc() -> Arc<dyn AgentDriver> {
        Arc::new(Self::new())
    }
}

/// The stdout stream names the thread once, on `thread.started`; later
/// envelopes carry no identity at all. A real driver would thread that through
/// its own session state — irrelevant to the transport under test, so the
/// fixture just falls back to a constant.
const FALLBACK_SESSION: &str = "codex-thread";

#[async_trait]
impl AgentDriver for CodexShapedDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &self.descriptor
    }

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::new([Capability::ProgressObservation, Capability::TurnBoundary])
    }

    fn spawn_invocation(&self, _: SpawnRequest<'_>) -> SpawnPlan {
        unimplemented!()
    }

    async fn provision_workspace(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
        unimplemented!()
    }

    async fn teardown_workspace(&self, _: Option<&Path>, _: &str) -> anyhow::Result<()> {
        unimplemented!()
    }

    async fn write_permission_config(&self, _: &PermissionInput, _: &Path) -> anyhow::Result<PermissionArtifacts> {
        unimplemented!()
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, _: &ProgressObservationConfig) -> ProgressIngress {
        ProgressIngress::StdoutJsonl
    }

    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| NormalizeError::Malformed("expected JSON object".into()))?;
        let kind = obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or(NormalizeError::MissingField("type"))?;
        let session_id = obj
            .get("thread_id")
            .and_then(|v| v.as_str())
            .unwrap_or(FALLBACK_SESSION)
            .to_owned();
        let item_type = obj
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|v| v.as_str());

        Ok(match (kind, item_type) {
            ("thread.started", _) => WorkerEvent::SessionStart {
                session_id,
                source: SessionStartSource::Startup,
            },
            ("turn.started", _) => WorkerEvent::UserPromptSubmit {
                session_id,
                prompt: String::new(),
            },
            ("item.started", Some("command_execution")) => WorkerEvent::PreToolUse {
                session_id,
                tool_name: "Bash".to_owned(),
                tool_input: raw["item"]["command"].clone(),
            },
            ("item.completed", Some("command_execution")) => WorkerEvent::PostToolUse {
                session_id,
                tool_name: "Bash".to_owned(),
                tool_input: raw["item"]["command"].clone(),
                tool_response: raw["item"]["aggregated_output"].clone(),
            },
            ("turn.completed", _) => WorkerEvent::Stop {
                session_id,
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            },
            (other, _) => return Err(NormalizeError::UnknownEvent(other.to_owned())),
        })
    }

    fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd> {
        // Codex's native `turn.completed` event normalises to this Stop shape.
        // Unlike Claude, Codex has no continuation signal, and its stdout
        // envelope does not distinguish a more specific end reason.
        match event {
            WorkerEvent::Stop {
                session_id,
                stop_reason,
                ..
            } => Some(TurnEnd {
                session_id: session_id.clone(),
                reason: *stop_reason,
                continuation: false,
            }),
            _ => None,
        }
    }

    fn tool_use_interception_wiring(&self, _: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        unimplemented!()
    }

    fn agent_rules_preamble(&self) -> &'static str {
        ""
    }

    fn transcript_path_for_session(&self, raw: &serde_json::Value) -> Option<String> {
        let path = raw.get("transcript_path")?.as_str()?;
        if path.is_empty() { None } else { Some(path.to_owned()) }
    }

    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        raw
    }

    fn extract_error_from_transcript(&self, _: &[serde_json::Value]) -> Option<String> {
        None
    }

    fn classify_error(&self, _: &str) -> WorkerErrorClass {
        unimplemented!()
    }

    fn structured_output_fallback(&self, _: StructuredOutputKind, _: &str) -> Vec<FallbackCandidate> {
        Vec::new()
    }
}

// ─── Fixtures ───────────────────────────────────────────────────────────────

/// One complete `codex exec --json` turn, verbatim from the investigation
/// doc's reproduction 1.
const CODEX_TURN: &str = concat!(
    r#"{"type":"thread.started","thread_id":"019f974c-3d59-7533-b320-3963123c809b"}"#,
    "\n",
    r#"{"type":"turn.started"}"#,
    "\n",
    r#"{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"/bin/zsh -lc 'echo hooktest-exec'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
    "\n",
    r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"/bin/zsh -lc 'echo hooktest-exec'","aggregated_output":"hooktest-exec\n","exit_code":0,"status":"completed"}}"#,
    "\n",
    r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"hooktest-exec"}}"#,
    "\n",
    r#"{"type":"turn.completed","usage":{"input_tokens":25699,"cached_input_tokens":14080,"cache_write_input_tokens":0,"output_tokens":37,"reasoning_output_tokens":0}}"#,
    "\n",
);

/// Drain a reader over an in-memory stream, returning the events and the
/// final stats.
async fn drain(stream: &'static str) -> (Vec<WorkerEvent>, ReaderStats) {
    drain_bytes(stream.as_bytes().to_vec()).await
}

async fn drain_bytes(bytes: Vec<u8>) -> (Vec<WorkerEvent>, ReaderStats) {
    let mut reader = StdoutJsonlProgressReader::new(bytes.as_slice(), CodexShapedDriver::arc());
    let mut events = Vec::new();
    while let Some(envelope) = reader.next_event().await {
        events.push(envelope.event);
    }
    (events, reader.stats())
}

fn kinds(events: &[WorkerEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            WorkerEvent::SessionStart { .. } => "session_start",
            WorkerEvent::UserPromptSubmit { .. } => "user_prompt_submit",
            WorkerEvent::PreToolUse { .. } => "pre_tool_use",
            WorkerEvent::PostToolUse { .. } => "post_tool_use",
            WorkerEvent::Stop { .. } => "stop",
            WorkerEvent::Notification { .. } => "notification",
            WorkerEvent::SessionEnd { .. } => "session_end",
        })
        .collect()
}

// ─── Happy path ─────────────────────────────────────────────────────────────

/// The load-bearing case: a real turn's stdout produces the same event shapes
/// the activity machine already consumes from the hook path — a lifecycle
/// start, a turn start, a tool pair, and an authoritative turn boundary.
#[tokio::test]
async fn parses_a_codex_turn_into_activity_events() {
    let (events, stats) = drain(CODEX_TURN).await;

    assert_eq!(
        kinds(&events),
        vec![
            "session_start",
            "user_prompt_submit",
            "pre_tool_use",
            "post_tool_use",
            "stop"
        ],
    );
    assert_eq!(stats.lines_read, 6);
    assert_eq!(stats.events_emitted, 5);
    // The `agent_message` item the driver has no mapping for.
    assert_eq!(stats.unrecognised_envelopes, 1);
    assert_eq!(stats.non_json_lines, 0);
}

#[test]
fn codex_turn_completed_is_a_non_continuation_turn_boundary() {
    let driver = CodexShapedDriver::new();
    let event = driver
        .normalize_progress_event(&serde_json::json!({"type": "turn.completed", "usage": {}}))
        .expect("Codex turn.completed must normalise");

    assert_eq!(
        driver.turn_boundary(&event),
        Some(TurnEnd {
            session_id: FALLBACK_SESSION.to_owned(),
            reason: boss_protocol::StopReason::Completed,
            continuation: false,
        }),
    );
    assert!(
        driver
            .turn_boundary(&WorkerEvent::UserPromptSubmit {
                session_id: FALLBACK_SESSION.to_owned(),
                prompt: String::new(),
            })
            .is_none()
    );
}

#[tokio::test]
async fn session_identity_comes_from_the_stream() {
    let (events, _) = drain(CODEX_TURN).await;
    let WorkerEvent::SessionStart { session_id, source } = &events[0] else {
        panic!("expected SessionStart, got {:?}", events[0]);
    };
    assert_eq!(session_id, "019f974c-3d59-7533-b320-3963123c809b");
    assert_eq!(*source, SessionStartSource::Startup);
}

#[tokio::test]
async fn empty_stream_yields_no_events() {
    let (events, stats) = drain("").await;
    assert!(events.is_empty());
    assert_eq!(stats, ReaderStats::default());
}

#[tokio::test]
async fn transcript_path_is_surfaced_through_the_driver() {
    let line = concat!(
        r#"{"type":"thread.started","thread_id":"t","transcript_path":"/home/u/.codex/sessions/t.jsonl"}"#,
        "\n",
    );
    let mut reader = StdoutJsonlProgressReader::new(line.as_bytes(), CodexShapedDriver::arc());
    let envelope = reader.next_event().await.expect("one envelope");
    assert_eq!(
        envelope.transcript_path.as_deref(),
        Some("/home/u/.codex/sessions/t.jsonl"),
    );
}

#[tokio::test]
async fn missing_transcript_path_is_none() {
    let mut reader = StdoutJsonlProgressReader::new(CODEX_TURN.as_bytes(), CodexShapedDriver::arc());
    let envelope = reader.next_event().await.expect("one envelope");
    assert!(envelope.transcript_path.is_none());
}

// ─── Tolerance: unknown variants and added fields ───────────────────────────

/// The Codex stream has no schema version field and has gained variants across
/// minor releases without announcement. An unmappable variant must cost one
/// skipped line, not the rest of the stream.
#[tokio::test]
async fn unknown_variants_are_skipped_not_fatal() {
    let stream = concat!(
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"item.updated","item":{"id":"item_9","type":"some_future_thing"}}"#,
        "\n",
        r#"{"type":"thread.resumed","thread_id":"t"}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{}}"#,
        "\n",
    );
    let (events, stats) = drain(stream).await;

    assert_eq!(kinds(&events), vec!["user_prompt_submit", "stop"]);
    assert_eq!(stats.unrecognised_envelopes, 2);
    assert_eq!(stats.lines_read, 4);
}

/// Added fields on an otherwise-known variant must not disturb decoding — the
/// driver reads what it knows and ignores the rest.
#[tokio::test]
async fn added_fields_on_a_known_variant_still_decode() {
    let stream = concat!(
        r#"{"type":"turn.completed","usage":{},"rate_limit":{"remaining":42},"unannounced":true}"#,
        "\n",
    );
    let (events, stats) = drain(stream).await;
    assert_eq!(kinds(&events), vec!["stop"]);
    assert_eq!(stats.unrecognised_envelopes, 0);
}

/// Valid JSON that isn't an object at all (a bare number, string, or array) is
/// the driver's `Malformed` case, tolerated like any other rejection.
#[tokio::test]
async fn non_object_json_is_skipped() {
    let stream = concat!(
        "42\n",
        "\"a bare string\"\n",
        "[1,2,3]\n",
        "null\n",
        r#"{"type":"turn.completed"}"#,
        "\n",
    );
    let (events, stats) = drain(stream).await;
    assert_eq!(kinds(&events), vec!["stop"]);
    assert_eq!(stats.unrecognised_envelopes, 4);
}

// ─── Tolerance: non-JSON, blank, and invalid-UTF-8 lines ────────────────────

/// Workers write plenty to stdout that is not a progress envelope — build
/// output, warnings, progress bars. All of it is skipped.
#[tokio::test]
async fn interleaved_non_json_output_is_skipped() {
    let stream = concat!(
        "Reading prompt from stdin...\n",
        r#"{"type":"turn.started"}"#,
        "\n",
        "warning: something happened\n",
        "{ this is not json at all\n",
        r#"{"type":"turn.completed","usage":{}}"#,
        "\n",
        "done.\n",
    );
    let (events, stats) = drain(stream).await;

    assert_eq!(kinds(&events), vec!["user_prompt_submit", "stop"]);
    assert_eq!(stats.non_json_lines, 4);
    assert_eq!(stats.lines_read, 6);
}

#[tokio::test]
async fn blank_and_whitespace_lines_are_skipped() {
    let stream = concat!(
        "\n",
        "   \n",
        "\t\n",
        r#"{"type":"turn.completed","usage":{}}"#,
        "\n",
        "\n",
    );
    let (events, stats) = drain(stream).await;
    assert_eq!(kinds(&events), vec!["stop"]);
    assert_eq!(stats.blank_lines, 4);
    assert_eq!(stats.non_json_lines, 0);
}

/// A stray non-UTF-8 byte must not end the stream. Lossy decoding turns it
/// into a parse failure, which is skipped like any other non-JSON line.
#[tokio::test]
async fn invalid_utf8_is_skipped_not_fatal() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0xff, 0xfe, 0x80]);
    bytes.push(b'\n');
    bytes.extend_from_slice(br#"{"type":"turn.completed","usage":{}}"#);
    bytes.push(b'\n');

    let (events, stats) = drain_bytes(bytes).await;
    assert_eq!(kinds(&events), vec!["stop"]);
    assert_eq!(stats.non_json_lines, 1);
}

/// A CRLF-terminated stream (a worker whose stdout passes through a pty or a
/// Windows-ish tool) parses: the `\r` is trimmed before the JSON parse.
#[tokio::test]
async fn crlf_terminated_lines_parse() {
    let stream = concat!(
        r#"{"type":"turn.started"}"#,
        "\r\n",
        r#"{"type":"turn.completed"}"#,
        "\r\n"
    );
    let (events, _) = drain(stream).await;
    assert_eq!(kinds(&events), vec!["user_prompt_submit", "stop"]);
}

// ─── Tolerance: framing across reads and at end of stream ───────────────────

/// A pipe hands back whatever bytes happen to be available, so an envelope
/// routinely arrives split across reads. Only a newline ends a line.
#[tokio::test]
async fn envelopes_split_across_reads_are_reassembled() {
    let (client, server) = tokio::io::duplex(8);
    let writer = tokio::spawn(async move {
        let mut client = client;
        // Deliberately chop mid-token, mid-string, and mid-newline.
        for chunk in [
            r#"{"type":"turn."#,
            r#"started"}"#,
            "\n{\"type\":\"turn.comp",
            "leted\",\"usage\":{}}",
            "\n",
        ] {
            client.write_all(chunk.as_bytes()).await.unwrap();
            client.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
    });

    let mut reader = StdoutJsonlProgressReader::new(server, CodexShapedDriver::arc());
    let mut events = Vec::new();
    while let Some(envelope) = reader.next_event().await {
        events.push(envelope.event);
    }
    writer.await.unwrap();

    assert_eq!(kinds(&events), vec!["user_prompt_submit", "stop"]);
    assert_eq!(reader.stats().non_json_lines, 0);
}

/// A killed worker closes the pipe mid-envelope. The complete lines before it
/// must still have been delivered, and the fragment must be dropped quietly.
#[tokio::test]
async fn stream_closing_mid_envelope_drops_the_fragment() {
    let stream = concat!(
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"item.started","item":{"id":"item_2","type":"command_exec"#,
    );
    let (events, stats) = drain(stream).await;

    assert_eq!(kinds(&events), vec!["user_prompt_submit"]);
    assert_eq!(stats.unterminated_tails, 1);
    assert_eq!(stats.non_json_lines, 1);
}

/// The mirror case: a *complete* final envelope that simply lacks a trailing
/// newline (the writer exited cleanly right after the last write) is still
/// delivered rather than discarded as a fragment.
#[tokio::test]
async fn complete_final_envelope_without_a_newline_is_delivered() {
    let stream = concat!(
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{}}"#
    );
    let (events, stats) = drain(stream).await;

    assert_eq!(kinds(&events), vec!["user_prompt_submit", "stop"]);
    assert_eq!(stats.unterminated_tails, 1);
    assert_eq!(stats.non_json_lines, 0);
}

// ─── Tolerance: unbounded lines ─────────────────────────────────────────────

/// A worker that dumps a huge blob onto stdout must not be able to make the
/// engine buffer it without limit. The over-long line is discarded whole and
/// the reader resynchronises on the next one.
#[tokio::test]
async fn oversized_line_is_discarded_and_the_stream_resynchronises() {
    let mut stream = String::new();
    stream.push_str(r#"{"type":"turn.started"}"#);
    stream.push('\n');
    stream.push_str(&format!(r#"{{"type":"item.completed","blob":"{}"}}"#, "x".repeat(4096)));
    stream.push('\n');
    stream.push_str(r#"{"type":"turn.completed","usage":{}}"#);
    stream.push('\n');

    let bytes = stream.into_bytes();
    let mut reader = StdoutJsonlProgressReader::with_config(
        bytes.as_slice(),
        CodexShapedDriver::arc(),
        ReaderConfig { max_line_bytes: 256 },
    );
    let mut events = Vec::new();
    while let Some(envelope) = reader.next_event().await {
        events.push(envelope.event);
    }

    assert_eq!(kinds(&events), vec!["user_prompt_submit", "stop"]);
    assert_eq!(reader.stats().oversized_lines, 1);
    // The discarded line was never framed, so it isn't counted as read.
    assert_eq!(reader.stats().lines_read, 2);
}

/// An over-long line that runs all the way to end of stream — a worker
/// emitting a newline-free blob until it dies — must terminate, not spin.
#[tokio::test]
async fn oversized_line_running_to_end_of_stream_terminates() {
    let bytes = "x".repeat(4096).into_bytes();
    let mut reader = StdoutJsonlProgressReader::with_config(
        bytes.as_slice(),
        CodexShapedDriver::arc(),
        ReaderConfig { max_line_bytes: 64 },
    );

    assert!(reader.next_event().await.is_none());
    assert_eq!(reader.stats().oversized_lines, 1);
    assert_eq!(reader.stats().lines_read, 0);
}

/// The same ceiling must hold when the over-long line arrives in pieces rather
/// than in one read — the reader has to notice it is over budget while
/// accumulating, not only when it finally sees the newline.
#[tokio::test]
async fn oversized_line_split_across_reads_is_discarded() {
    let (client, server) = tokio::io::duplex(16);
    let writer = tokio::spawn(async move {
        let mut client = client;
        for _ in 0..40 {
            client.write_all(&b"y".repeat(16)).await.unwrap();
            client.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
        client.write_all(b"\n").await.unwrap();
        client
            .write_all(b"{\"type\":\"turn.completed\",\"usage\":{}}\n")
            .await
            .unwrap();
        client.flush().await.unwrap();
    });

    let mut reader =
        StdoutJsonlProgressReader::with_config(server, CodexShapedDriver::arc(), ReaderConfig { max_line_bytes: 64 });
    let mut events = Vec::new();
    while let Some(envelope) = reader.next_event().await {
        events.push(envelope.event);
    }
    writer.await.unwrap();

    assert_eq!(kinds(&events), vec!["stop"]);
    assert_eq!(reader.stats().oversized_lines, 1);
}

#[tokio::test]
async fn default_config_allows_a_realistically_large_envelope() {
    // Codex's biggest observed envelope is a `command_execution` carrying the
    // command's whole aggregated output. 64 KiB of it must sail through the
    // default ceiling untouched.
    let payload = "o".repeat(64 * 1024);
    let stream = format!(
        r#"{{"type":"item.completed","item":{{"id":"i","type":"command_execution","command":"x","aggregated_output":"{payload}"}}}}"#,
    ) + "\n";

    let (events, stats) = drain_bytes(stream.into_bytes()).await;
    assert_eq!(kinds(&events), vec!["post_tool_use"]);
    assert_eq!(stats.oversized_lines, 0);
}

// ─── Tolerance: I/O errors ───────────────────────────────────────────────────

/// An `AsyncRead` that hands back a fixed prefix and then fails every
/// subsequent `poll_read` with a caller-chosen error kind. Models a worker
/// whose pipe breaks mid-stream (a killed process, a closed SSH channel).
struct FailAfterPrefix {
    prefix: Vec<u8>,
    pos: usize,
    kind: std::io::ErrorKind,
}

impl FailAfterPrefix {
    fn new(prefix: &str, kind: std::io::ErrorKind) -> Self {
        Self {
            prefix: prefix.as_bytes().to_vec(),
            pos: 0,
            kind,
        }
    }
}

impl tokio::io::AsyncRead for FailAfterPrefix {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let remaining = &self.prefix[self.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::task::Poll::Ready(Err(std::io::Error::from(self.kind)))
    }
}

/// A broken pipe must end the stream cleanly rather than hang or panic: the
/// envelopes read before the break are delivered, `ended_with_io_error` is
/// set, and `next_event` returns `None` instead of looping forever.
#[tokio::test]
async fn io_error_ends_the_stream_cleanly_and_is_recorded() {
    let stream = FailAfterPrefix::new(
        concat!(r#"{"type":"turn.started"}"#, "\n"),
        std::io::ErrorKind::BrokenPipe,
    );
    let mut reader = StdoutJsonlProgressReader::new(stream, CodexShapedDriver::arc());

    let mut events = Vec::new();
    while let Some(envelope) = reader.next_event().await {
        events.push(envelope.event);
    }

    assert_eq!(kinds(&events), vec!["user_prompt_submit"]);
    assert!(reader.stats().ended_with_io_error);
    assert!(reader.next_event().await.is_none(), "must stay ended, not loop");
}

/// An `AsyncRead` whose first `poll_read` fails with `Interrupted` once, then
/// serves real bytes. `Interrupted` must be retried transparently — it is not
/// a stream-ending error — so the envelope after it still decodes.
struct InterruptedOnce {
    fired: bool,
    rest: Vec<u8>,
    pos: usize,
}

impl InterruptedOnce {
    fn new(bytes: &str) -> Self {
        Self {
            fired: false,
            rest: bytes.as_bytes().to_vec(),
            pos: 0,
        }
    }
}

impl tokio::io::AsyncRead for InterruptedOnce {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if !self.fired {
            self.fired = true;
            return std::task::Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::Interrupted)));
        }
        let remaining = &self.rest[self.pos..];
        let n = remaining.len().min(buf.remaining());
        buf.put_slice(&remaining[..n]);
        self.pos += n;
        std::task::Poll::Ready(Ok(()))
    }
}

/// If the `Interrupted` retry in `next_raw_line` ever regressed to falling
/// through to the fatal-error branch, this would drop the entire stream and
/// silently emit nothing — the assertion on the emitted event is what would
/// catch that.
#[tokio::test]
async fn interrupted_read_is_retried_not_treated_as_fatal() {
    let stream = InterruptedOnce::new(concat!(r#"{"type":"turn.completed","usage":{}}"#, "\n"));
    let mut reader = StdoutJsonlProgressReader::new(stream, CodexShapedDriver::arc());

    let envelope = reader
        .next_event()
        .await
        .expect("interrupted read must be retried, not fatal");
    assert_eq!(kinds(&[envelope.event]), vec!["stop"]);
    assert!(!reader.stats().ended_with_io_error);
}
