//! Mapping between the wire `WorkerEvent` and [`boss_protocol::WorkerEvent`]:
//! [`to_wire`] and [`from_wire`]. (Both types are foreign to this crate, so
//! the conversion cannot be a `From`/`TryFrom` impl.)
//!
//! The wire message mirrors the seven hook variants without reducing tool
//! values to strings: `input_json` / `response_json` carry the plugin-specific
//! JSON objects as bytes. `serde_json::Value::Null` is encoded as empty bytes
//! and empty bytes decode to `Null`, which keeps a `PreToolUse` (no response)
//! symmetrical with its domain form.
//!
//! Decoding is deliberately conservative. A stop reason or start source this
//! build does not know maps to `Other`, never to `Completed`: an unknown stop
//! reason must not be mistaken for completion evidence. The wire `TurnStop`
//! transcript watermark has no domain counterpart and is dropped here; a
//! consumer that needs it reads the wire message.

use boss_protocol::{SessionStartSource, StopReason, WorkerEvent as Domain};

use crate::v1::{self, worker_event::Event};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    #[error("worker event has no event body")]
    MissingEvent,
    #[error("worker event has an empty session id")]
    MissingSessionId,
    #[error("{field} is not valid JSON: {detail}")]
    InvalidJson { field: &'static str, detail: String },
}

/// Encodes a domain event as its wire form. The wire `TurnStop` transcript
/// watermark is left unset; the spoke stamps it after encoding.
pub fn to_wire(event: &Domain) -> v1::WorkerEvent {
    let (session_id, event) = match event {
        Domain::SessionStart {
            session_id,
            source,
            model,
        } => (
            session_id,
            Event::SessionStart(v1::SessionStart {
                source: start_source_to_wire(*source) as i32,
                model: model.clone(),
            }),
        ),
        Domain::UserPromptSubmit { session_id, prompt } => (session_id, Event::UserPrompt(prompt.clone())),
        Domain::PreToolUse {
            session_id,
            tool_name,
            tool_input,
        } => (
            session_id,
            Event::PreToolUse(v1::ToolUse {
                name: tool_name.clone(),
                input_json: json_to_bytes(tool_input),
                response_json: Vec::new(),
            }),
        ),
        Domain::PostToolUse {
            session_id,
            tool_name,
            tool_input,
            tool_response,
        } => (
            session_id,
            Event::PostToolUse(v1::ToolUse {
                name: tool_name.clone(),
                input_json: json_to_bytes(tool_input),
                response_json: json_to_bytes(tool_response),
            }),
        ),
        Domain::Stop {
            session_id,
            stop_hook_active,
            stop_reason,
        } => (
            session_id,
            Event::Stop(v1::TurnStop {
                stop_hook_active: *stop_hook_active,
                reason: stop_reason_to_wire(*stop_reason) as i32,
                transcript_generation: String::new(),
                transcript_through: 0,
            }),
        ),
        Domain::Notification { session_id, message } => (session_id, Event::Notification(message.clone())),
        Domain::ToolPoll {
            session_id,
            awaiting_model,
        } => (session_id, Event::ToolPoll(*awaiting_model)),
        Domain::ModelProgress { session_id } => (session_id, Event::ModelProgress(true)),
        Domain::SessionEnd { session_id, reason } => (session_id, Event::SessionEnd(reason.clone())),
    };
    v1::WorkerEvent {
        session_id: session_id.clone(),
        event: Some(event),
    }
}

/// Decodes a wire event, failing closed on a missing body, an empty session id
/// or malformed tool JSON.
pub fn from_wire(wire: v1::WorkerEvent) -> Result<Domain, ConversionError> {
    let v1::WorkerEvent { session_id, event } = wire;
    if session_id.is_empty() {
        return Err(ConversionError::MissingSessionId);
    }
    Ok(match event.ok_or(ConversionError::MissingEvent)? {
        Event::SessionStart(start) => Domain::SessionStart {
            session_id,
            source: start_source_from_wire(start.source),
            model: start.model.filter(|model| !model.is_empty()),
        },
        Event::UserPrompt(prompt) => Domain::UserPromptSubmit { session_id, prompt },
        Event::PreToolUse(tool) => Domain::PreToolUse {
            session_id,
            tool_name: tool.name,
            tool_input: json_from_bytes("tool.input_json", &tool.input_json)?,
        },
        Event::PostToolUse(tool) => Domain::PostToolUse {
            session_id,
            tool_name: tool.name,
            tool_input: json_from_bytes("tool.input_json", &tool.input_json)?,
            tool_response: json_from_bytes("tool.response_json", &tool.response_json)?,
        },
        Event::Stop(stop) => Domain::Stop {
            session_id,
            stop_hook_active: stop.stop_hook_active,
            stop_reason: stop_reason_from_wire(stop.reason),
        },
        Event::Notification(message) => Domain::Notification { session_id, message },
        Event::ToolPoll(awaiting_model) => Domain::ToolPoll {
            session_id,
            awaiting_model,
        },
        Event::ModelProgress(_) => Domain::ModelProgress { session_id },
        Event::SessionEnd(reason) => Domain::SessionEnd { session_id, reason },
    })
}

fn json_to_bytes(value: &serde_json::Value) -> Vec<u8> {
    match value {
        serde_json::Value::Null => Vec::new(),
        other => other.to_string().into_bytes(),
    }
}

fn json_from_bytes(field: &'static str, bytes: &[u8]) -> Result<serde_json::Value, ConversionError> {
    if bytes.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(bytes).map_err(|err| ConversionError::InvalidJson {
        field,
        detail: err.to_string(),
    })
}

fn start_source_to_wire(source: SessionStartSource) -> v1::StartSource {
    match source {
        SessionStartSource::Startup => v1::StartSource::Startup,
        SessionStartSource::Resume => v1::StartSource::Resume,
        SessionStartSource::Clear => v1::StartSource::Clear,
        SessionStartSource::Compact => v1::StartSource::Compact,
        SessionStartSource::Other => v1::StartSource::OtherStart,
    }
}

fn start_source_from_wire(raw: i32) -> SessionStartSource {
    match v1::StartSource::try_from(raw) {
        Ok(v1::StartSource::Startup) => SessionStartSource::Startup,
        Ok(v1::StartSource::Resume) => SessionStartSource::Resume,
        Ok(v1::StartSource::Clear) => SessionStartSource::Clear,
        Ok(v1::StartSource::Compact) => SessionStartSource::Compact,
        Ok(v1::StartSource::OtherStart | v1::StartSource::StartUnspecified) | Err(_) => SessionStartSource::Other,
    }
}

fn stop_reason_to_wire(reason: StopReason) -> v1::StopReason {
    match reason {
        StopReason::Completed => v1::StopReason::Completed,
        StopReason::AwaitingInput => v1::StopReason::AwaitingInput,
        StopReason::Interrupted => v1::StopReason::Interrupted,
        StopReason::Other => v1::StopReason::OtherStop,
    }
}

fn stop_reason_from_wire(raw: i32) -> StopReason {
    match v1::StopReason::try_from(raw) {
        Ok(v1::StopReason::Completed) => StopReason::Completed,
        Ok(v1::StopReason::AwaitingInput) => StopReason::AwaitingInput,
        Ok(v1::StopReason::Interrupted) => StopReason::Interrupted,
        Ok(v1::StopReason::OtherStop | v1::StopReason::StopUnspecified) | Err(_) => StopReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn round_trip(event: Domain) -> Domain {
        from_wire(to_wire(&event)).expect("decodes")
    }

    #[test]
    fn every_variant_round_trips_through_the_wire_form() {
        let events = [
            Domain::ModelProgress { session_id: "s".into() },
            Domain::ToolPoll {
                session_id: "s".into(),
                awaiting_model: true,
            },
            Domain::ToolPoll {
                session_id: "s".into(),
                awaiting_model: false,
            },
            Domain::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Compact,
                model: Some("m".into()),
            },
            Domain::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Other,
                model: None,
            },
            Domain::UserPromptSubmit {
                session_id: "s".into(),
                prompt: "go".into(),
            },
            Domain::PreToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: json!({"command": "ls"}),
            },
            Domain::PostToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: json!({"command": "ls", "n": [1, 2, {"deep": null}]}),
                tool_response: json!({"stdout": "a\nb", "ok": true}),
            },
            Domain::Stop {
                session_id: "s".into(),
                stop_hook_active: true,
                stop_reason: StopReason::AwaitingInput,
            },
            Domain::Notification {
                session_id: "s".into(),
                message: "needs input".into(),
            },
            Domain::SessionEnd {
                session_id: "s".into(),
                reason: "logout".into(),
            },
        ];
        for event in events {
            assert_eq!(round_trip(event.clone()), event);
        }
    }

    #[test]
    fn an_absent_tool_value_is_null_in_both_directions() {
        let event = Domain::PreToolUse {
            session_id: "s".into(),
            tool_name: "Read".into(),
            tool_input: serde_json::Value::Null,
        };
        let wire = to_wire(&event);
        let Some(Event::PreToolUse(tool)) = &wire.event else {
            panic!("wrong variant")
        };
        assert!(tool.input_json.is_empty() && tool.response_json.is_empty());
        assert_eq!(from_wire(wire), Ok(event));
    }

    #[test]
    fn unknown_stop_reasons_never_become_completion() {
        for raw in [0, 99, -1] {
            let wire = v1::WorkerEvent {
                session_id: "s".into(),
                event: Some(Event::Stop(v1::TurnStop {
                    reason: raw,
                    ..Default::default()
                })),
            };
            let Ok(Domain::Stop { stop_reason, .. }) = from_wire(wire) else {
                panic!("expected a stop")
            };
            assert_eq!(stop_reason, StopReason::Other, "raw {raw}");
        }
    }

    #[test]
    fn unknown_start_sources_decode_as_other() {
        for raw in [0, 99] {
            let wire = v1::WorkerEvent {
                session_id: "s".into(),
                event: Some(Event::SessionStart(v1::SessionStart {
                    source: raw,
                    model: Some(String::new()),
                })),
            };
            assert_eq!(
                from_wire(wire),
                Ok(Domain::SessionStart {
                    session_id: "s".into(),
                    source: SessionStartSource::Other,
                    model: None
                })
            );
        }
    }

    #[test]
    fn malformed_events_are_errors() {
        let no_body = v1::WorkerEvent {
            session_id: "s".into(),
            event: None,
        };
        assert_eq!(from_wire(no_body), Err(ConversionError::MissingEvent));

        let no_session = v1::WorkerEvent {
            session_id: String::new(),
            event: Some(Event::UserPrompt("x".into())),
        };
        assert_eq!(from_wire(no_session), Err(ConversionError::MissingSessionId));

        let bad_json = v1::WorkerEvent {
            session_id: "s".into(),
            event: Some(Event::PostToolUse(v1::ToolUse {
                name: "Bash".into(),
                input_json: b"{not json".to_vec(),
                response_json: Vec::new(),
            })),
        };
        assert!(matches!(
            from_wire(bad_json),
            Err(ConversionError::InvalidJson {
                field: "tool.input_json",
                ..
            })
        ));
    }
}
