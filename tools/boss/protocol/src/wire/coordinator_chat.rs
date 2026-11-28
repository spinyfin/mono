//! Coordinator-chat RPC surface: the one request, its result, and the
//! topic-update body published on [`crate::TOPIC_COORDINATOR_CHAT`].
//!
//! Re-exported from `wire`. Chat *items* (messages, tool calls, notices) live
//! in [`crate::types::coordinator_chat`]; this module is the engine-to-app
//! control and push envelope around them.

use serde::{Deserialize, Serialize};

use crate::{
    CoordinatorChatConnectionState, CoordinatorChatInProgressMessage, CoordinatorChatItem, CoordinatorChatMessageDelta,
};

/// One operation on [`crate::FrontendRequest::CoordinatorChat`].
///
/// Flattened onto the request with tag `op`, so the wire form is
/// `{"type":"coordinator_chat","op":"send",…}` rather than a nested
/// `operation` object. App-only at the tier gate and again in the handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CoordinatorChatOperation {
    /// Queue an operator message. `client_message_id` is the app's idempotency
    /// key for this send; attachments are local file paths, not base64.
    Send {
        text: String,
        client_message_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<CoordinatorChatSendAttachment>,
    },
    /// Cancel the in-flight coordinator turn (`session/cancel`).
    Cancel,
    /// Choose one option on a pending permission request.
    AnswerPermission { request_id: String, option_id: String },
    /// Return the conversation items created or changed after journal record
    /// `after_seq` (items with `updated_seq > after_seq`, so a tool call
    /// started before that record and finished after it is included), plus the
    /// current connection state and any in-progress streaming snapshot.
    Fetch { after_seq: u64 },
}

/// A file the app named in a send. The engine validates and stores it; the
/// existing work-item attachment store is not used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorChatSendAttachment {
    pub path: String,
}

/// An operator message Stop took out of the queue so the app can put it
/// back in the composer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorChatReturnedDraft {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
    pub text: String,
}

/// Snapshot returned by [`CoordinatorChatOperation::Fetch`].
///
/// Boxed inside [`CoordinatorChatOutcome::Fetch`] so that enum stays within
/// clippy's `large_enum_variant` budget; serde output is identical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorChatFetchOutcome {
    pub generation: String,
    pub items: Vec<CoordinatorChatItem>,
    pub last_seq: u64,
    pub state: CoordinatorChatConnectionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_progress: Option<CoordinatorChatInProgressMessage>,
}

/// Outcome of [`crate::FrontendRequest::CoordinatorChat`], carried on
/// [`crate::FrontendEvent::CoordinatorChatResult`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordinatorChatOutcome {
    /// The chat backend is not running. The panel shows `reason` and offers
    /// switching to the terminal coordinator.
    BackendNotRunning { reason: String },
    /// The send was accepted into the intent log under `client_message_id`.
    Sent { client_message_id: String },
    /// The in-flight turn was cancelled, or there was nothing to cancel.
    /// Operator messages that were still queued are in `returned_to_composer`
    /// so the app can put them back in the composer; the field is omitted
    /// when there are none, so an empty cancel stays `{"type":"cancelled"}`.
    Cancelled {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        returned_to_composer: Vec<CoordinatorChatReturnedDraft>,
    },
    /// The named permission request was answered with the chosen option.
    PermissionAnswered { request_id: String },
    /// Snapshot of items after the requested sequence number.
    Fetch(Box<CoordinatorChatFetchOutcome>),
}

impl CoordinatorChatOutcome {
    /// Stop outcome carrying the operator drafts that were still queued.
    pub fn cancelled(drafts: impl IntoIterator<Item = (Option<String>, String)>) -> Self {
        Self::Cancelled {
            returned_to_composer: drafts
                .into_iter()
                .map(|(client_message_id, text)| CoordinatorChatReturnedDraft {
                    client_message_id,
                    text,
                })
                .collect(),
        }
    }
}

/// Body of a [`crate::TopicEventPayload::CoordinatorChat`] push: one
/// generation plus either a completed item, a streaming delta, or a
/// connection-state change.
///
/// A failed start is a state change whose phase is
/// [`crate::CoordinatorChatConnectionPhase::Failed`] with the reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordinatorChatTopicUpdate {
    Item { item: CoordinatorChatItem },
    Delta { delta: CoordinatorChatMessageDelta },
    State { state: CoordinatorChatConnectionState },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_omits_empty_drafts_on_the_wire() {
        let value = serde_json::to_value(CoordinatorChatOutcome::cancelled(std::iter::empty())).unwrap();
        assert_eq!(value, serde_json::json!({"type": "cancelled"}));
    }

    #[test]
    fn cancelled_carries_returned_drafts() {
        let value = serde_json::to_value(CoordinatorChatOutcome::cancelled([(
            Some("cm-1".to_owned()),
            "queued".to_owned(),
        )]))
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "cancelled",
                "returned_to_composer": [{"client_message_id": "cm-1", "text": "queued"}],
            })
        );
    }
}
