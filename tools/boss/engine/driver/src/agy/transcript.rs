//! Antigravity native-transcript reshape.
//!
//! `agy` writes `transcript_full.jsonl` under
//! `$HOME/.gemini/antigravity-cli/brain/<cid>/.system_generated/logs/`
//! (pane viability study; the `Stop` payload stamps `transcriptPath`).
//! Captured rows look like:
//!
//! ```json
//! {"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","content":"AFTER_ESC_OK"}
//! ```
//!
//! The live-status summariser and the blocked-marker parser expect the
//! canonical Claude-shaped assistant record. This module maps native
//! planner replies onto that shape. A later transcript-access task
//! replaces the skeleton fixture with a captured run.

use serde_json::{Value, json};

/// Native step types whose `content` is the worker's own prose.
fn is_assistant_step(step_type: &str) -> bool {
    matches!(step_type, "PLANNER_RESPONSE" | "planner_response")
}

fn content_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    if let Some(arr) = value.as_array() {
        let mut parts = Vec::new();
        for block in arr {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                parts.push(text);
            } else if let Some(text) = block.as_str() {
                parts.push(text);
            }
        }
        if parts.is_empty() {
            return None;
        }
        return Some(parts.join(""));
    }
    value.get("text").and_then(Value::as_str).map(str::to_owned)
}

fn canonical_assistant(text: String) -> Value {
    json!({
        "type": "assistant",
        "content": [{"type": "text", "text": text}],
    })
}

/// Reshape one native `transcript_full.jsonl` record to the canonical
/// assistant/user shape. Already-canonical records pass through.
pub(super) fn normalize_transcript_entry(raw: Value) -> Value {
    let Some(obj) = raw.as_object() else {
        return raw;
    };
    if obj.get("type").and_then(Value::as_str) == Some("assistant") {
        return raw;
    }
    let step_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    if (is_assistant_step(step_type) || obj.get("role").and_then(Value::as_str) == Some("model"))
        && let Some(text) = obj.get("content").and_then(content_text)
    {
        return canonical_assistant(text);
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_response_becomes_canonical_assistant() {
        let raw = json!({
            "step_index": 5,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "content": "[blocked] reason=\"need a decision\"",
        });
        assert_eq!(
            normalize_transcript_entry(raw),
            json!({
                "type": "assistant",
                "content": [{"type": "text", "text": "[blocked] reason=\"need a decision\""}],
            }),
        );
    }

    #[test]
    fn already_canonical_assistant_passes_through() {
        let raw = json!({
            "type": "assistant",
            "content": [{"type": "text", "text": "already canonical"}],
        });
        assert_eq!(normalize_transcript_entry(raw.clone()), raw);
    }

    #[test]
    fn user_input_is_left_alone() {
        let raw = json!({
            "step_index": 0,
            "type": "USER_INPUT",
            "status": "DONE",
            "content": "do the work",
        });
        assert_eq!(normalize_transcript_entry(raw.clone()), raw);
    }
}
