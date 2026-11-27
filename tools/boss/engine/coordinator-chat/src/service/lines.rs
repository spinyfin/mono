//! The lines the service writes to the agent host.
//!
//! Message bodies come from the official ACP schema types, so field names and
//! the method name of each request cannot drift from the protocol. Only the
//! JSON-RPC envelope is assembled here: Boss owns its own line framing and
//! request-id table (coordinator ACP spike, S15), so the typed connection of
//! the SDK is not used.

use boss_acp_client::JsonRpcMessage;
use boss_acp_client::schema::{
    CancelNotification, ContentBlock, PromptRequest, RequestId, RequestPermissionOutcome, RequestPermissionResponse,
    SelectedPermissionOutcome, TextContent,
};
use serde_json::{Value, json};

/// A `session/prompt` request carrying `text`.
pub(super) fn prompt_line(id: i64, session_id: &str, text: &str) -> String {
    let request = PromptRequest::new(session_id.to_owned(), vec![ContentBlock::Text(TextContent::new(text))]);
    let (method, params) = parts(&request);
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// The `session/cancel` notification. It has no id and no response.
pub(super) fn cancel_line(session_id: &str) -> String {
    let (method, params) = parts(&CancelNotification::new(session_id.to_owned()));
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

/// The client's answer to the agent's `session/request_permission` request
/// `id`.
///
/// `id` is the agent's own request id, exactly as it arrived: the agent
/// numbers its requests from 0, independently of the client, so the id alone
/// does not say whose request it is. A response travels in the opposite
/// direction to its request, which is how the reducer and the journal tell the
/// two apart.
pub(super) fn permission_response_line(id: &RequestId, outcome: PermissionAnswer<'_>) -> String {
    let outcome = match outcome {
        PermissionAnswer::Selected(option_id) => {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id.to_owned()))
        }
        PermissionAnswer::Cancelled => RequestPermissionOutcome::Cancelled,
    };
    let result = serde_json::to_value(RequestPermissionResponse::new(outcome)).unwrap_or(Value::Null);
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// How the operator, or Stop, answered a permission request.
#[derive(Debug, Clone, Copy)]
pub(super) enum PermissionAnswer<'a> {
    Selected(&'a str),
    /// The prompt turn was cancelled before the operator answered.
    Cancelled,
}

fn parts(message: &impl JsonRpcMessage) -> (String, Value) {
    // Encoding a request built from owned strings cannot fail; an empty
    // object would be refused by the agent rather than acted on.
    message
        .to_untyped_message()
        .map(|untyped| untyped.into_parts())
        .unwrap_or_else(|_| (String::new(), Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).expect("a line is JSON")
    }

    #[test]
    fn a_prompt_is_a_text_block_for_the_session() {
        assert_eq!(
            parse(&prompt_line(7, "sess-1", "hello \"there\"\nsecond line")),
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "session/prompt",
                "params": {
                    "sessionId": "sess-1",
                    "prompt": [{"type": "text", "text": "hello \"there\"\nsecond line"}],
                },
            })
        );
    }

    #[test]
    fn a_line_never_contains_a_raw_newline() {
        // The host frames by `\n`: a newline inside the text must stay escaped.
        assert!(!prompt_line(1, "s", "a\nb\r\nc").contains('\n'));
    }

    #[test]
    fn cancel_is_a_notification_without_an_id() {
        assert_eq!(
            parse(&cancel_line("sess-1")),
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "sess-1"}})
        );
    }

    #[test]
    fn a_selected_option_answers_the_agents_request_with_the_agents_id() {
        assert_eq!(
            parse(&permission_response_line(
                &RequestId::Number(0),
                PermissionAnswer::Selected("allow-once")
            )),
            json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}},
            })
        );
    }

    #[test]
    fn a_string_request_id_stays_a_string() {
        let line = permission_response_line(&RequestId::Str("0".into()), PermissionAnswer::Cancelled);
        assert_eq!(parse(&line)["id"], json!("0"));
        assert_eq!(parse(&line)["result"], json!({"outcome": {"outcome": "cancelled"}}));
    }
}
