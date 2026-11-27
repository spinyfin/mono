//! Boss-owned vocabulary for the native SwiftUI coordinator chat panel.
//!
//! These types define the engine-to-app conversation projection: connection
//! state, ordered chat items, and the in-progress streaming snapshot/delta
//! shapes. They are Boss's own vocabulary rather than ACP's schema so the
//! wire contract with the app stays stable if the underlying backend changes.

use serde::{Deserialize, Serialize};

/// Authentication state reported by the coordinator's agent backend.
///
/// Account notifications from the adapter carry signed-in identity details
/// (email, organization, plan). Only the coarse state crosses into Boss's
/// vocabulary; account content is never stored here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorChatAuthState {
    #[default]
    Unknown,
    Authenticated,
    Authenticating,
    Unauthenticated,
    #[serde(other)]
    Other,
}

/// High-level activity phase of the coordinator chat session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum CoordinatorChatConnectionPhase {
    #[default]
    Starting,
    Initializing,
    Idle,
    Working,
    WaitingForPermission,
    Failed {
        reason: String,
    },
    #[serde(other)]
    Unknown,
}

/// Connection and session state reduced from the coordinator's wire journal.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CoordinatorChatConnectionState {
    #[serde(default)]
    #[builder(default)]
    pub auth_state: CoordinatorChatAuthState,
    #[serde(default)]
    #[builder(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    #[builder(default)]
    pub last_seq: u64,
    #[serde(default)]
    #[builder(default)]
    pub phase: CoordinatorChatConnectionPhase,
    #[serde(default)]
    #[builder(default)]
    pub supports_async_tasks: bool,
    #[serde(default)]
    #[builder(default)]
    pub supports_background_subagents: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<CoordinatorChatUsage>,
}

/// Snapshot of a message currently being streamed by the coordinator.
///
/// Streaming text is not stored per chunk in the completed item list; this
/// snapshot carries only the redacted prefix that is safe to expose (excluding
/// any held-back credential-redaction tail).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorChatInProgressMessage {
    /// Stable item id that the finalized message item will carry once complete.
    pub item_id: String,
    /// Journal sequence number of the first chunk in this message.
    pub started_seq: u64,
    /// Latest journal sequence number that contributed to this snapshot.
    pub updated_seq: u64,
    /// Role of the streaming message (typically `Coordinator`).
    pub role: CoordinatorChatMessageRole,
    /// Redacted text accumulated so far that is safe to display.
    pub text: String,
}

/// One ordered item in the coordinator conversation projection.
///
/// Item ids and ordering derive from journal sequence numbers (`seq`), so a
/// fetch after a sequence number is stable across engine restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorChatItem {
    /// Deterministic id derived from the journal sequence number that created the item.
    pub id: String,
    /// Journal sequence number that created this item (ordering key).
    pub seq: u64,
    /// Latest journal sequence number that created or mutated this item.
    pub updated_seq: u64,
    /// Wall-clock milliseconds from the creating journal record (display only).
    pub ts_ms: u64,
    /// Typed payload for this item.
    pub kind: CoordinatorChatItemKind,
}

/// Discriminated payload for a [`CoordinatorChatItem`].
///
/// Unrecognized item types deserialize to [`Self::Unknown`] so an older app
/// renders a future item kind as a collapsed generic row without failing decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordinatorChatItemKind {
    Message(CoordinatorChatMessageItem),
    Thought(CoordinatorChatThoughtItem),
    ToolCall(CoordinatorChatToolCallItem),
    PermissionRequest(CoordinatorChatPermissionRequestItem),
    TurnBoundary(CoordinatorChatTurnBoundaryItem),
    Notice(CoordinatorChatNoticeItem),
    Usage(CoordinatorChatUsage),
    #[serde(other)]
    Unknown,
}

/// Incremental redacted text delta for an in-progress streamed message.
///
/// `offset` is the UTF-8 byte offset within the accumulated safe redacted text
/// at which `text` appends, letting the app detect dropped pushes and re-fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorChatMessageDelta {
    /// Stable item id matching [`CoordinatorChatInProgressMessage::item_id`].
    pub item_id: String,
    /// Journal sequence number of the record that produced this delta.
    pub seq: u64,
    /// UTF-8 byte offset in the accumulated safe text where `text` starts.
    pub offset: usize,
    /// Newly safe-to-display redacted UTF-8 text appended at `offset`.
    pub text: String,
}

/// A completed conversational message block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorChatMessageItem {
    pub role: CoordinatorChatMessageRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
}

/// Who produced a conversational message item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorChatMessageRole {
    Operator,
    Coordinator,
    Engine,
    Peer,
    #[serde(other)]
    Unknown,
}

/// One-line or divider notice in the conversation timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorChatNoticeItem {
    pub notice_kind: CoordinatorChatNoticeKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

/// Category of a [`CoordinatorChatNoticeItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorChatNoticeKind {
    SessionStarted,
    Compaction,
    Error,
    LoginRequired,
    Info,
    #[serde(other)]
    Unknown,
}

/// One selectable option presented on a permission request card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorChatPermissionOption {
    pub option_id: String,
    pub option_kind: CoordinatorChatPermissionOptionKind,
    pub name: String,
}

/// Semantic category of a [`CoordinatorChatPermissionOption`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorChatPermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    #[default]
    #[serde(other)]
    Other,
}

/// A permission request issued by the agent, together with its resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CoordinatorChatPermissionRequestItem {
    /// Agent-assigned JSON-RPC request id (stringified for wire transport).
    pub request_id: String,
    pub options: Vec<CoordinatorChatPermissionOption>,
    pub resolution: CoordinatorChatPermissionResolution,
    pub title: String,
    #[serde(default)]
    #[builder(default)]
    pub tool_kind: CoordinatorChatToolKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Lifecycle state of a [`CoordinatorChatPermissionRequestItem`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CoordinatorChatPermissionResolution {
    #[default]
    Pending,
    Selected {
        option_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option_name: Option<String>,
    },
    /// Resolved without an operator choice, either because the turn was
    /// cancelled by the client or because the agent withdrew the request with
    /// `$/cancel_request` (`withdrawn = true`). Both render as cancelled.
    Cancelled {
        #[serde(default)]
        withdrawn: bool,
    },
    #[serde(other)]
    Unknown,
}

/// A collapsed reasoning / thought block produced by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorChatThoughtItem {
    pub text: String,
}

/// A generic tool call row with bounded, redacted input and output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CoordinatorChatToolCallItem {
    pub tool_call_id: String,
    #[serde(default)]
    #[builder(default)]
    pub blocked_by_auto_mode: bool,
    #[serde(default)]
    #[builder(default)]
    pub status: CoordinatorChatToolStatus,
    pub title: String,
    #[serde(default)]
    #[builder(default)]
    pub tool_kind: CoordinatorChatToolKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_execution_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

/// Broad category of a tool invocation for icon and row treatment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorChatToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[default]
    #[serde(other)]
    Other,
}

/// Execution status of a [`CoordinatorChatToolCallItem`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorChatToolStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
    #[serde(other)]
    Unknown,
}

/// Marks the completion of a prompt turn or an agent-started autonomous turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorChatTurnBoundaryItem {
    /// True when the turn was opened by the adapter's `async_task_spawned`
    /// (`taskType: "turn"`) extension rather than a client `session/prompt`.
    #[serde(default)]
    pub agent_initiated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Populated from the turn's last `usage_update` (`_meta._claude/origin`),
    /// which may arrive after the turn closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<CoordinatorChatTurnOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

/// What triggered a coordinator turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorChatTurnOrigin {
    Human,
    Engine,
    Peer,
    TaskNotification,
    #[serde(other)]
    Unknown,
}

/// Context window and cost snapshot reported by the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorChatUsage {
    pub size: u64,
    pub used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_amount: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_currency: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_item_kind_deserializes_to_unknown_variant() {
        let raw = json!({
            "id": "future-99",
            "seq": 99,
            "updated_seq": 99,
            "ts_ms": 1_795_000_000_000u64,
            "kind": {
                "type": "structured_card_v2",
                "work_item_id": "task_123",
            }
        });
        let item: CoordinatorChatItem = serde_json::from_value(raw).expect("unknown item kind must decode");
        assert_eq!(item.id, "future-99");
        assert_eq!(item.seq, 99);
        assert_eq!(item.kind, CoordinatorChatItemKind::Unknown);
    }

    #[test]
    fn tool_call_and_permission_items_round_trip() {
        let tool = CoordinatorChatItem {
            id: "tool-10".to_owned(),
            seq: 10,
            updated_seq: 12,
            ts_ms: 1_795_000_000_010,
            kind: CoordinatorChatItemKind::ToolCall(
                CoordinatorChatToolCallItem::builder()
                    .tool_call_id("toolu_1")
                    .title("boss product list --json")
                    .tool_kind(CoordinatorChatToolKind::Execute)
                    .status(CoordinatorChatToolStatus::Completed)
                    .tool_name("Bash")
                    .input(r#"{"command":"boss product list --json"}"#)
                    .output("```json\n[]\n```")
                    .build(),
            ),
        };
        let encoded = serde_json::to_value(&tool).unwrap();
        assert_eq!(encoded["kind"]["type"], "tool_call");
        assert_eq!(encoded["kind"]["tool_kind"], "execute");
        let decoded: CoordinatorChatItem = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, tool);

        let perm = CoordinatorChatItem {
            id: "perm-20".to_owned(),
            seq: 20,
            updated_seq: 22,
            ts_ms: 1_795_000_000_020,
            kind: CoordinatorChatItemKind::PermissionRequest(
                CoordinatorChatPermissionRequestItem::builder()
                    .request_id("0")
                    .title("touch probe.txt")
                    .tool_kind(CoordinatorChatToolKind::Execute)
                    .options(vec![CoordinatorChatPermissionOption {
                        option_id: "allow-once".to_owned(),
                        option_kind: CoordinatorChatPermissionOptionKind::AllowOnce,
                        name: "Yes".to_owned(),
                    }])
                    .resolution(CoordinatorChatPermissionResolution::Cancelled { withdrawn: true })
                    .build(),
            ),
        };
        let encoded = serde_json::to_value(&perm).unwrap();
        assert_eq!(encoded["kind"]["type"], "permission_request");
        assert_eq!(encoded["kind"]["resolution"]["status"], "cancelled");
        assert_eq!(encoded["kind"]["resolution"]["withdrawn"], true);
        let decoded: CoordinatorChatItem = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, perm);
    }

    #[test]
    fn turn_boundary_and_notice_round_trip() {
        let boundary = CoordinatorChatItem {
            id: "turn-44".to_owned(),
            seq: 44,
            updated_seq: 45,
            ts_ms: 1_795_000_000_044,
            kind: CoordinatorChatItemKind::TurnBoundary(CoordinatorChatTurnBoundaryItem {
                agent_initiated: true,
                error: None,
                origin: Some(CoordinatorChatTurnOrigin::TaskNotification),
                stop_reason: Some("completed".to_owned()),
            }),
        };
        let encoded = serde_json::to_value(&boundary).unwrap();
        assert_eq!(encoded["kind"]["type"], "turn_boundary");
        assert_eq!(encoded["kind"]["origin"], "task_notification");
        assert_eq!(
            serde_json::from_value::<CoordinatorChatItem>(encoded).unwrap(),
            boundary
        );

        let notice = CoordinatorChatItem {
            id: "notice-50".to_owned(),
            seq: 50,
            updated_seq: 51,
            ts_ms: 1_795_000_000_050,
            kind: CoordinatorChatItemKind::Notice(CoordinatorChatNoticeItem {
                notice_kind: CoordinatorChatNoticeKind::Compaction,
                text: "Context compacted (manual: 42602 -> 7073 tokens)".to_owned(),
                post_tokens: Some(7073),
                pre_tokens: Some(42602),
                trigger: Some("manual".to_owned()),
            }),
        };
        let encoded = serde_json::to_value(&notice).unwrap();
        assert_eq!(encoded["kind"]["type"], "notice");
        assert_eq!(encoded["kind"]["notice_kind"], "compaction");
        assert_eq!(serde_json::from_value::<CoordinatorChatItem>(encoded).unwrap(), notice);
    }
}
