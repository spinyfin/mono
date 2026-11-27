//! Deterministic conversation reducer from wire-journal records to Boss's
//! coordinator chat vocabulary.
//!
//! The reducer is a pure state machine: feeding the same sequence of
//! [`WireRecord`]s joined with the engine's append-only [`IntentRecord`] log
//! always yields the exact same [`CoordinatorChatConnectionState`], ordered
//! [`CoordinatorChatItem`] list, [`CoordinatorChatInProgressMessage`]
//! snapshot, and [`TurnQueue`] state. It runs identically on live traffic and
//! on journal replay.

use std::collections::HashMap;
use std::io::{self, BufRead};

use boss_acp_client::JsonRpcMessage;
use boss_acp_client::schema::{
    ContentBlock, PermissionOptionKind, PromptResponse, RequestId, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SessionNotification, SessionUpdate, StopReason, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolKind, UsageUpdate,
};
use boss_agent_host::journal::{Direction, WireRecord, read_records};
use boss_protocol::{
    CoordinatorChatAuthState, CoordinatorChatConnectionPhase, CoordinatorChatConnectionState,
    CoordinatorChatInProgressMessage, CoordinatorChatItem, CoordinatorChatItemKind, CoordinatorChatMessageDelta,
    CoordinatorChatMessageItem, CoordinatorChatMessageRole, CoordinatorChatNoticeItem, CoordinatorChatNoticeKind,
    CoordinatorChatPermissionOption, CoordinatorChatPermissionOptionKind, CoordinatorChatPermissionRequestItem,
    CoordinatorChatPermissionResolution, CoordinatorChatThoughtItem, CoordinatorChatToolCallItem,
    CoordinatorChatToolKind, CoordinatorChatToolStatus, CoordinatorChatTurnBoundaryItem, CoordinatorChatTurnOrigin,
    CoordinatorChatUsage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::intent::{IntentRecord, IntentRecordKind};
use crate::queue::{PromptContent, QueuedPrompt, TurnQueue};
use crate::redact::{CredentialRedactor, StreamingRedactor, StreamingRedactorSnapshot};

/// Default character ceiling for bounded tool inputs and outputs.
///
/// Matches the spike probe's event bound (`acp_client/src/bin/probe/events.rs`).
/// Redaction always runs before bounding so a credential straddling the cut
/// point cannot leak in part.
pub const DEFAULT_TOOL_TEXT_LIMIT: usize = 4_000;

/// Summary of what a single [`ConversationReducer::apply_record`] or
/// [`ConversationReducer::apply_intent_record`] call changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReduceDelta {
    /// Item ids created or mutated by this step, in order.
    pub upserted_item_ids: Vec<String>,
    /// Newly safe-to-display redacted delta for the in-progress message, if any.
    pub message_delta: Option<CoordinatorChatMessageDelta>,
    /// True when [`ConversationReducer::state`] changed during this step.
    pub state_changed: bool,
}

/// Pure, deterministic reducer from [`WireRecord`]s to coordinator chat state.
#[derive(Debug, Clone)]
pub struct ConversationReducer {
    redactor: CredentialRedactor,
    state: CoordinatorChatConnectionState,
    items: ItemStore,
    wire: WireTracker,
    queue_state: QueueTracker,
}

#[derive(Debug, Clone, Default)]
struct QueueTracker {
    queue: TurnQueue,
    last_intent_seq: u64,
}

#[derive(Debug, Clone, Default)]
struct ItemIndices {
    tool: HashMap<String, usize>,
    compaction: HashMap<String, usize>,
    permission: HashMap<String, usize>,
}

#[derive(Debug, Clone)]
struct ItemStore {
    items: Vec<CoordinatorChatItem>,
    indices: ItemIndices,
    streaming: StreamingBuffers,
    tool_text_limit: usize,
}

impl ItemStore {
    fn new(tool_text_limit: usize) -> Self {
        Self {
            items: Vec::new(),
            indices: ItemIndices::default(),
            streaming: StreamingBuffers::default(),
            tool_text_limit,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct StreamingBuffers {
    message: Option<ActiveStream>,
    thought: Option<ActiveStream>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreamMeta {
    pub(crate) item_id: String,
    pub(crate) started_seq: u64,
    pub(crate) updated_seq: u64,
    pub(crate) ts_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ActiveStream {
    meta: StreamMeta,
    buffer: StreamingRedactor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActiveStreamSnapshot {
    pub(crate) meta: StreamMeta,
    pub(crate) buffer: StreamingRedactorSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingClientRequest {
    pub(crate) request_id: RequestId,
    pub(crate) method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TurnOriginTracker {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) current_turn_origin: Option<CoordinatorChatTurnOrigin>,
    #[serde(default)]
    pub(crate) current_turn_origin_from_intent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_turn_boundary_idx: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireTracker {
    #[serde(default)]
    pub(crate) pending_client_requests: HashMap<String, PendingClientRequest>,
    #[serde(default)]
    pub(crate) pending_agent_permissions: HashMap<String, String>,
    #[serde(flatten)]
    pub(crate) turns: TurnOriginTracker,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) highest_numeric_client_request_id: Option<i64>,
}

impl ConversationReducer {
    /// Creates a reducer configured with `redactor` and [`DEFAULT_TOOL_TEXT_LIMIT`].
    pub fn new(redactor: CredentialRedactor) -> Self {
        Self::with_tool_text_limit(redactor, DEFAULT_TOOL_TEXT_LIMIT)
    }

    /// Creates a reducer with a custom character ceiling for tool input/output.
    pub fn with_tool_text_limit(redactor: CredentialRedactor, tool_text_limit: usize) -> Self {
        Self {
            redactor,
            state: CoordinatorChatConnectionState::default(),
            items: ItemStore::new(tool_text_limit),
            wire: WireTracker::default(),
            queue_state: QueueTracker::default(),
        }
    }

    /// Current connection state.
    pub fn state(&self) -> &CoordinatorChatConnectionState {
        &self.state
    }

    /// Ordered completed (and in-place-updated) chat items.
    pub fn items(&self) -> &[CoordinatorChatItem] {
        &self.items.items
    }

    /// Current turn queue state recovered from intent and wire records.
    pub fn queue(&self) -> &TurnQueue {
        &self.queue_state.queue
    }

    /// Highest wire-journal sequence number applied so far (`0` if none).
    pub fn last_wire_seq(&self) -> u64 {
        self.state.last_seq
    }

    /// Highest intent-log sequence number applied so far (`0` if none).
    pub fn last_intent_seq(&self) -> u64 {
        self.queue_state.last_intent_seq
    }

    /// Client JSON-RPC request ids still awaiting a response from the agent.
    ///
    /// On engine restart, register each of these with
    /// `boss_acp_client::connection::PreparedConnection::expect_response`
    /// before reading live traffic from the agent host.
    pub fn pending_client_request_ids(&self) -> Vec<RequestId> {
        let mut pairs: Vec<(&String, &RequestId)> = self
            .wire
            .pending_client_requests
            .iter()
            .map(|(key, req)| (key, &req.request_id))
            .collect();
        pairs.sort_by_key(|(key, _)| *key);
        pairs.into_iter().map(|(_, id)| id.clone()).collect()
    }

    /// Agent JSON-RPC permission request ids still awaiting a client response.
    ///
    /// Per ACP, Stop answers each of these with `cancelled` before sending
    /// `session/cancel`.
    pub fn pending_permission_request_ids(&self) -> Vec<RequestId> {
        let mut keys: Vec<&String> = self.wire.pending_agent_permissions.keys().collect();
        keys.sort();
        keys.into_iter()
            .filter_map(|key| serde_json::from_str::<RequestId>(key).ok())
            .collect()
    }

    /// Highest numeric client `RequestId` observed on the wire or in an
    /// `IntentRecord::Sent` record so far, useful for seeding a fresh
    /// connection's id counter after a restart so no previously logged request
    /// id (including an abandoned unsent attempt) is ever reused.
    pub fn highest_numeric_client_request_id(&self) -> Option<i64> {
        self.wire.highest_numeric_client_request_id
    }

    fn observe_client_request_id(&mut self, id: &RequestId) {
        if let RequestId::Number(num) = id {
            self.wire.highest_numeric_client_request_id = Some(
                self.wire
                    .highest_numeric_client_request_id
                    .map_or(*num, |prev| prev.max(*num)),
            );
        }
    }

    /// Applies one append-only record from the generation's intent log.
    ///
    /// Records whose `seq` is already covered (`seq > 0 && seq <= last_intent_seq`)
    /// are ignored so replaying overlapping tails is idempotent.
    pub fn apply_intent_record(&mut self, record: &IntentRecord) -> ReduceDelta {
        if record.seq > 0 && record.seq <= self.queue_state.last_intent_seq {
            return ReduceDelta::default();
        }
        if record.seq > self.queue_state.last_intent_seq {
            self.queue_state.last_intent_seq = record.seq;
        }
        match &record.kind {
            IntentRecordKind::Queued {
                intent_id,
                origin,
                text,
                client_message_id,
                images,
            } => {
                self.queue_state.queue.enqueue(QueuedPrompt {
                    intent_id: *intent_id,
                    origin: *origin,
                    content: PromptContent {
                        text: text.clone(),
                        images: images.clone(),
                    },
                    client_message_id: client_message_id.clone(),
                    queued_ts_ms: record.ts_ms,
                });
            }
            IntentRecordKind::Sent { intent_id, request_id } => {
                self.observe_client_request_id(request_id);
                self.queue_state
                    .queue
                    .mark_intent_sent(*intent_id, request_id.clone(), record.ts_ms);
            }
            IntentRecordKind::Requeued { intent_id } => {
                self.queue_state.queue.apply_requeued_intent(*intent_id);
            }
            IntentRecordKind::Withdrawn { intent_id, .. } => {
                self.queue_state.queue.apply_withdrawn_intent(*intent_id);
            }
        }
        ReduceDelta::default()
    }

    /// Re-queues any prompts marked `Sent` in the intent log that never
    /// appeared on the wire journal (for example when the previous engine died
    /// between writing the intent record and the host journalling the line).
    pub fn reconcile_unsent_dispatches(&mut self) -> Vec<QueuedPrompt> {
        self.queue_state.queue.recover_unsent_dispatches()
    }

    /// Snapshot of the currently streaming coordinator message, if any.
    ///
    /// Contains only the safe redacted prefix; any trailing characters held
    /// back for split-credential redaction are excluded.
    pub fn in_progress_message(&self) -> Option<CoordinatorChatInProgressMessage> {
        let active = self.items.streaming.message.as_ref()?;
        Some(CoordinatorChatInProgressMessage {
            item_id: active.meta.item_id.clone(),
            started_seq: active.meta.started_seq,
            updated_seq: active.meta.updated_seq,
            role: CoordinatorChatMessageRole::Coordinator,
            text: active.buffer.safe_text().to_owned(),
        })
    }

    /// Looks up an item by its stable id.
    pub fn item_by_id(&self, id: &str) -> Option<&CoordinatorChatItem> {
        self.items.items.iter().find(|item| item.id == id)
    }

    /// Inserts `item` into `self.items.items` in non-decreasing `seq` order,
    /// shifting any stored item indices at or after the insertion point.
    fn insert_item(&mut self, item: CoordinatorChatItem) -> usize {
        if self.items.items.last().is_none_or(|last| last.seq <= item.seq) {
            let idx = self.items.items.len();
            self.items.items.push(item);
            return idx;
        }
        let idx = self.items.items.partition_point(|existing| existing.seq <= item.seq);
        self.insert_item_at(item, idx)
    }

    fn insert_item_at(&mut self, item: CoordinatorChatItem, idx: usize) -> usize {
        self.items.items.insert(idx, item);
        for slot in self.items.indices.tool.values_mut() {
            if *slot >= idx {
                *slot += 1;
            }
        }
        for slot in self.items.indices.compaction.values_mut() {
            if *slot >= idx {
                *slot += 1;
            }
        }
        for slot in self.items.indices.permission.values_mut() {
            if *slot >= idx {
                *slot += 1;
            }
        }
        if let Some(last_idx) = &mut self.wire.turns.last_turn_boundary_idx
            && *last_idx >= idx
        {
            *last_idx += 1;
        }
        idx
    }

    /// Reduces one wire-journal record in place and reports what changed.
    pub fn apply_record(&mut self, record: &WireRecord) -> ReduceDelta {
        let mut delta = ReduceDelta::default();
        let previous_state = self.state.clone();

        if record.seq > self.state.last_seq {
            self.state.last_seq = record.seq;
        }

        if record.invalid.is_some() {
            delta.state_changed = self.state != previous_state;
            return delta;
        }

        let Ok(Value::Object(message)) = serde_json::from_str::<Value>(&record.line) else {
            delta.state_changed = self.state != previous_state;
            return delta;
        };

        match record.dir {
            Direction::ToAgent => self.handle_to_agent(record, &message, &mut delta),
            Direction::FromAgent => self.handle_from_agent(record, &message, &mut delta),
        }

        self.redactor.redact_connection_state(&mut self.state);
        delta.state_changed = self.state != previous_state;
        delta
    }

    fn handle_to_agent(&mut self, record: &WireRecord, message: &Map<String, Value>, delta: &mut ReduceDelta) {
        let id = parse_request_id(message.get("id"));
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if let Some(id) = id {
                self.handle_client_request(record, &id, method, message.get("params"), delta);
            } else if method == "session/cancel" {
                if self.has_active_turn() {
                    self.state.cancel_requested = true;
                }
                self.queue_state.queue.note_wire_cancel_sent();
            }
        } else if let Some(id) = id
            && (message.contains_key("result") || message.contains_key("error"))
        {
            self.handle_client_response(record, &id, message, delta);
        }
    }

    fn handle_client_request(
        &mut self,
        record: &WireRecord,
        id: &RequestId,
        method: &str,
        params: Option<&Value>,
        delta: &mut ReduceDelta,
    ) {
        self.observe_client_request_id(id);
        let detail = match method {
            "session/set_mode" => params
                .and_then(|value| value.get("modeId"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        };
        self.wire.pending_client_requests.insert(
            request_id_key(id),
            PendingClientRequest {
                request_id: id.clone(),
                method: method.to_owned(),
                detail,
            },
        );
        match method {
            "initialize" => {
                if self.state.phase == CoordinatorChatConnectionPhase::Starting {
                    self.state.phase = CoordinatorChatConnectionPhase::Initializing;
                }
            }
            "session/prompt" => {
                // Prompts are not echoed by the adapter, so operator/engine
                // message items and turn origins come from joining this wire
                // request id against the engine's intent log.
                self.flush_streams(record.seq, delta);
                self.wire.turns.last_turn_boundary_idx = None;
                if let Some(in_flight) = self.queue_state.queue.on_wire_prompt_sent(id, record.seq) {
                    let item_id = format!("prompt-{}", record.seq);
                    let mut item = CoordinatorChatItem {
                        id: item_id.clone(),
                        seq: record.seq,
                        updated_seq: record.seq,
                        ts_ms: record.ts_ms,
                        kind: CoordinatorChatItemKind::Message(CoordinatorChatMessageItem {
                            role: in_flight.prompt.origin.message_role(),
                            text: self.redactor.redact_str(in_flight.prompt.text()),
                            client_message_id: in_flight.prompt.client_message_id.clone(),
                            message_id: None,
                            sender: None,
                        }),
                    };
                    self.redactor.redact_item(&mut item);
                    let idx = self.items.items.partition_point(|existing| {
                        existing.seq < record.seq || (existing.seq == record.seq && existing.id.starts_with("prompt-"))
                    });
                    self.insert_item_at(item, idx);
                    delta.upserted_item_ids.push(item_id);
                    self.wire.turns.current_turn_origin = Some(in_flight.prompt.origin.turn_origin());
                    self.wire.turns.current_turn_origin_from_intent = true;
                } else {
                    // Not joined to any intent (for example a prompt sent
                    // outside the intent log): the adapter is still running
                    // it, so the queue must stay busy until its response.
                    self.queue_state.queue.on_wire_prompt_unbound(id);
                    self.wire.turns.current_turn_origin = None;
                    self.wire.turns.current_turn_origin_from_intent = false;
                }
                self.recompute_phase();
            }
            _ => {}
        }
    }

    fn handle_client_response(
        &mut self,
        record: &WireRecord,
        id: &RequestId,
        message: &Map<String, Value>,
        delta: &mut ReduceDelta,
    ) {
        let Some(display_id) = self.wire.pending_agent_permissions.remove(&request_id_key(id)) else {
            return;
        };
        let resolution = if let Some(result) = message.get("result") {
            match serde_json::from_value::<RequestPermissionResponse>(result.clone()) {
                Ok(response) => match response.outcome {
                    RequestPermissionOutcome::Selected(selected) => {
                        let option_id = selected.option_id.to_string();
                        let option_name = self.lookup_permission_option_name(&display_id, &option_id);
                        CoordinatorChatPermissionResolution::Selected { option_id, option_name }
                    }
                    RequestPermissionOutcome::Cancelled => {
                        CoordinatorChatPermissionResolution::Cancelled { withdrawn: false }
                    }
                    _ => CoordinatorChatPermissionResolution::Unknown,
                },
                Err(_) => CoordinatorChatPermissionResolution::Cancelled { withdrawn: false },
            }
        } else {
            CoordinatorChatPermissionResolution::Cancelled { withdrawn: false }
        };
        self.resolve_permission_item(&display_id, resolution, record.seq, delta);
        self.recompute_phase();
    }

    fn handle_from_agent(&mut self, record: &WireRecord, message: &Map<String, Value>, delta: &mut ReduceDelta) {
        let id = parse_request_id(message.get("id"));
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let params = message.get("params").unwrap_or(&Value::Null);
            if let Some(id) = id {
                self.handle_agent_request(record, &id, method, params, delta);
            } else {
                self.handle_agent_notification(record, method, params, delta);
            }
        } else if let Some(id) = id
            && (message.contains_key("result") || message.contains_key("error"))
        {
            self.handle_agent_response(record, &id, message, delta);
        }
    }

    fn handle_agent_request(
        &mut self,
        record: &WireRecord,
        id: &RequestId,
        method: &str,
        params: &Value,
        delta: &mut ReduceDelta,
    ) {
        if method != "session/request_permission" {
            return;
        }
        let Ok(request) = RequestPermissionRequest::parse_message(method, params) else {
            return;
        };
        let tool_call_id = request.tool_call.tool_call_id.to_string();
        let is_child_permission = params
            .pointer("/toolCall/_meta/claudeCode/parentToolUseId")
            .or_else(|| params.pointer("/_meta/claudeCode/parentToolUseId"))
            .and_then(Value::as_str)
            .is_some()
            || self
                .items
                .indices
                .tool
                .get(&tool_call_id)
                .and_then(|&idx| self.items.items.get(idx))
                .is_some_and(|item| {
                    matches!(
                        &item.kind,
                        CoordinatorChatItemKind::ToolCall(tool) if tool.parent_tool_call_id.is_some()
                    )
                });
        // A background child's permission request interleaves with the
        // coordinator's own streaming and must not finalize the open stream.
        if !is_child_permission {
            self.flush_streams(record.seq, delta);
        }

        let display_id = id.to_string();
        let tool_call = &request.tool_call;
        let title = self
            .redactor
            .redact_str(tool_call.fields.title.as_deref().unwrap_or("Permission requested"));
        let tool_kind = tool_call.fields.kind.map(map_tool_kind).unwrap_or_default();
        let input = tool_call
            .fields
            .raw_input
            .as_ref()
            .filter(|value| !value.is_null())
            .map(|value| self.redact_and_bound(&compact_json(value)));
        let options = request
            .options
            .iter()
            .map(|option| CoordinatorChatPermissionOption {
                option_id: option.option_id.to_string(),
                option_kind: map_permission_option_kind(option.kind),
                name: self.redactor.redact_str(&option.name),
            })
            .collect();

        let item_id = format!("perm-{}", record.seq);
        let item = CoordinatorChatItem {
            id: item_id.clone(),
            seq: record.seq,
            updated_seq: record.seq,
            ts_ms: record.ts_ms,
            kind: CoordinatorChatItemKind::PermissionRequest(
                CoordinatorChatPermissionRequestItem::builder()
                    .request_id(display_id.clone())
                    .tool_call_id(tool_call_id)
                    .title(title)
                    .tool_kind(tool_kind)
                    .maybe_input(input)
                    .options(options)
                    .resolution(CoordinatorChatPermissionResolution::Pending)
                    .build(),
            ),
        };
        let index = self.insert_item(item);
        self.items.indices.permission.insert(display_id.clone(), index);
        self.wire
            .pending_agent_permissions
            .insert(request_id_key(id), display_id);
        delta.upserted_item_ids.push(item_id);
        self.recompute_phase();
    }

    fn handle_agent_notification(
        &mut self,
        record: &WireRecord,
        method: &str,
        params: &Value,
        delta: &mut ReduceDelta,
    ) {
        match method {
            "session/update" => self.handle_session_update(record, params, delta),
            "$/cancel_request" => self.handle_cancel_request(record, params, delta),
            "auth/status_update" | "_auth/status_update" => self.handle_auth_status_update(record, params, delta),
            _ => {}
        }
    }

    fn handle_cancel_request(&mut self, record: &WireRecord, params: &Value, delta: &mut ReduceDelta) {
        let Some(id) = parse_request_id(params.get("requestId")) else {
            return;
        };
        let Some(display_id) = self.wire.pending_agent_permissions.remove(&request_id_key(&id)) else {
            return;
        };
        self.resolve_permission_item(
            &display_id,
            CoordinatorChatPermissionResolution::Cancelled { withdrawn: true },
            record.seq,
            delta,
        );
        self.recompute_phase();
    }

    fn handle_auth_status_update(&mut self, record: &WireRecord, params: &Value, delta: &mut ReduceDelta) {
        // Account notifications carry signed-in email/organization/plan fields.
        // Only the coarse authentication state is retained; account content is
        // never read into connection state or chat items.
        let Some(raw_state) = params.get("state").and_then(Value::as_str) else {
            return;
        };
        let next = match raw_state {
            "authenticated" => CoordinatorChatAuthState::Authenticated,
            "authenticating" => CoordinatorChatAuthState::Authenticating,
            "unauthenticated" | "login_required" | "logged_out" => CoordinatorChatAuthState::Unauthenticated,
            _ => CoordinatorChatAuthState::Other,
        };
        let became_unauthenticated = next == CoordinatorChatAuthState::Unauthenticated && self.state.auth_state != next;
        self.state.auth_state = next;
        if became_unauthenticated {
            self.flush_streams(record.seq, delta);
            self.push_notice(
                record,
                CoordinatorChatNoticeKind::LoginRequired,
                "Claude login required".to_owned(),
                delta,
            );
        }
    }

    fn handle_agent_response(
        &mut self,
        record: &WireRecord,
        id: &RequestId,
        message: &Map<String, Value>,
        delta: &mut ReduceDelta,
    ) {
        let Some(pending) = self.wire.pending_client_requests.remove(&request_id_key(id)) else {
            return;
        };
        match pending.method.as_str() {
            "initialize" => self.handle_initialize_response(record, message, delta),
            "session/new" | "session/resume" => self.handle_session_new_response(record, message, delta),
            "session/set_mode" => {
                if message.contains_key("result")
                    && let Some(mode) = pending.detail
                {
                    self.state.mode = Some(self.redactor.redact_str(&mode));
                }
            }
            "session/set_config_option" => {
                if let Some(result) = message.get("result")
                    && let Some(model) = extract_model_from_config_options(result.get("configOptions"))
                {
                    self.state.model = Some(self.redactor.redact_str(&model));
                }
            }
            "session/prompt" => self.handle_prompt_response(record, id, message, delta),
            _ => {}
        }
    }

    fn handle_initialize_response(
        &mut self,
        record: &WireRecord,
        message: &Map<String, Value>,
        delta: &mut ReduceDelta,
    ) {
        if let Some(result) = message.get("result") {
            if let Some(info) = result.get("agentInfo") {
                self.state.agent_name = info
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| self.redactor.redact_str(name));
                self.state.agent_version = info
                    .get("version")
                    .and_then(Value::as_str)
                    .map(|version| self.redactor.redact_str(version));
            }
            if let Some(caps) = result
                .pointer("/_meta/jetbrains/air/capabilities")
                .and_then(Value::as_array)
            {
                self.state.supports_background_subagents =
                    caps.iter().any(|cap| cap.as_str() == Some("backgroundSubagents"));
                self.state.supports_async_tasks = caps.iter().any(|cap| cap.as_str() == Some("asyncTasks"));
            }
        } else if let Some(error) = message.get("error") {
            let reason = self.format_rpc_error("initialize failed", error);
            self.push_notice(record, CoordinatorChatNoticeKind::Error, reason.clone(), delta);
            self.state.phase = CoordinatorChatConnectionPhase::Failed { reason };
        }
    }

    fn handle_session_new_response(
        &mut self,
        record: &WireRecord,
        message: &Map<String, Value>,
        delta: &mut ReduceDelta,
    ) {
        if let Some(result) = message.get("result") {
            self.state.session_id = result.get("sessionId").and_then(Value::as_str).map(str::to_owned);
            if let Some(mode) = result.pointer("/modes/currentModeId").and_then(Value::as_str) {
                self.state.mode = Some(self.redactor.redact_str(mode));
            }
            if let Some(model) = extract_model_from_config_options(result.get("configOptions")) {
                self.state.model = Some(self.redactor.redact_str(&model));
            }
            self.push_notice(
                record,
                CoordinatorChatNoticeKind::SessionStarted,
                "Coordinator session started".to_owned(),
                delta,
            );
            // A fresh session clears any prior session-level failure state.
            self.state.phase = CoordinatorChatConnectionPhase::Idle;
            self.recompute_phase();
        } else if let Some(error) = message.get("error") {
            let code = error.get("code").and_then(Value::as_i64);
            let reason = self.format_rpc_error("session start failed", error);
            let kind = if is_login_required_error(code, &reason) {
                self.state.auth_state = CoordinatorChatAuthState::Unauthenticated;
                CoordinatorChatNoticeKind::LoginRequired
            } else {
                CoordinatorChatNoticeKind::Error
            };
            self.push_notice(record, kind, reason.clone(), delta);
            self.state.phase = CoordinatorChatConnectionPhase::Failed { reason };
        }
    }

    fn handle_prompt_response(
        &mut self,
        record: &WireRecord,
        id: &RequestId,
        message: &Map<String, Value>,
        delta: &mut ReduceDelta,
    ) {
        self.flush_streams(record.seq, delta);
        self.state.cancel_requested = false;
        self.queue_state.queue.on_wire_prompt_finished(id);
        let origin = self.wire.turns.current_turn_origin.take();
        self.wire.turns.current_turn_origin_from_intent = false;

        if let Some(result) = message.get("result") {
            let stop_reason = serde_json::from_value::<PromptResponse>(result.clone())
                .map(|response| map_stop_reason(response.stop_reason).to_owned())
                .unwrap_or_else(|_| {
                    result
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .map(|reason| self.redactor.redact_str(reason))
                        .unwrap_or_else(|| "end_turn".to_owned())
                });
            self.push_turn_boundary(record, false, origin, Some(stop_reason), None, delta);
            self.recompute_phase();
        } else if let Some(error) = message.get("error") {
            let code = error.get("code").and_then(Value::as_i64);
            let reason = self.format_rpc_error("turn failed", error);
            let notice_kind = if is_login_required_error(code, &reason) {
                self.state.auth_state = CoordinatorChatAuthState::Unauthenticated;
                CoordinatorChatNoticeKind::LoginRequired
            } else {
                CoordinatorChatNoticeKind::Error
            };
            self.push_notice(record, notice_kind, reason.clone(), delta);
            self.push_turn_boundary(
                record,
                false,
                origin,
                Some("error".to_owned()),
                Some(reason.clone()),
                delta,
            );
            if is_session_fatal_error(code, &reason) {
                self.state.phase = CoordinatorChatConnectionPhase::Failed { reason };
            } else {
                self.recompute_phase();
            }
        }
    }

    fn handle_session_update(&mut self, record: &WireRecord, params: &Value, delta: &mut ReduceDelta) {
        let raw_update = params.get("update").unwrap_or(&Value::Null);
        match SessionNotification::parse_message("session/update", params) {
            Ok(notification) => self.handle_typed_session_update(record, &notification.update, raw_update, delta),
            Err(_) => self.handle_untyped_session_update(record, raw_update, delta),
        }
    }

    fn handle_typed_session_update(
        &mut self,
        record: &WireRecord,
        update: &SessionUpdate,
        raw_update: &Value,
        delta: &mut ReduceDelta,
    ) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                self.flush_thought(record.seq, delta);
                if let ContentBlock::Text(text) = &chunk.content {
                    let message_id = chunk.message_id.as_ref().map(ToString::to_string);
                    self.push_message_chunk(record, &text.text, message_id, delta);
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                self.flush_message(record.seq, delta);
                if let ContentBlock::Text(text) = &chunk.content {
                    let message_id = chunk.message_id.as_ref().map(ToString::to_string);
                    self.push_thought_chunk(record, &text.text, message_id);
                }
            }
            SessionUpdate::UserMessageChunk(_) => {
                // Live prompts are not echoed on the wire; operator/engine
                // messages come from the engine's intent log.
            }
            SessionUpdate::ToolCall(call) => {
                self.handle_tool_call(record, call, raw_update, delta);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.handle_tool_call_update(record, update, raw_update, delta);
            }
            SessionUpdate::CurrentModeUpdate(mode) => {
                self.state.mode = Some(self.redactor.redact_str(mode.current_mode_id.0.as_ref()));
            }
            SessionUpdate::ConfigOptionUpdate(config) => {
                if let Some(model) = extract_model_from_typed_options(&config.config_options) {
                    self.state.model = Some(self.redactor.redact_str(&model));
                }
            }
            SessionUpdate::UsageUpdate(usage) => {
                self.handle_usage_update(record, usage, raw_update, delta);
            }
            SessionUpdate::CompactionUpdate(_) => {
                self.flush_streams(record.seq, delta);
                self.push_notice(
                    record,
                    CoordinatorChatNoticeKind::Compaction,
                    "Conversation compacted".to_owned(),
                    delta,
                );
            }
            SessionUpdate::Notice(_) => {
                if let Some(text) = raw_update
                    .get("message")
                    .or_else(|| raw_update.get("text"))
                    .and_then(Value::as_str)
                {
                    self.flush_streams(record.seq, delta);
                    let redacted = self.redactor.redact_str(text);
                    self.push_notice(record, CoordinatorChatNoticeKind::Info, redacted, delta);
                }
            }
            // AvailableCommandsUpdate, SessionInfoUpdate, Plan, etc. do not
            // open a turn or interrupt streaming text.
            _ => {}
        }
    }

    fn handle_untyped_session_update(&mut self, record: &WireRecord, raw_update: &Value, delta: &mut ReduceDelta) {
        let Some(kind) = raw_update.get("sessionUpdate").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "async_task_spawned" => {
                let task_type = raw_update.get("taskType").and_then(Value::as_str);
                let task_id = raw_update.get("asyncTaskId").and_then(Value::as_str);
                // Only `taskType: "turn"` opens an agent-started turn; background
                // shell/subagent tasks do not open a turn on their own.
                if task_type == Some("turn")
                    && let Some(task_id) = task_id
                {
                    self.flush_streams(record.seq, delta);
                    self.queue_state.queue.on_agent_turn_started(task_id);
                    self.wire.turns.last_turn_boundary_idx = None;
                    self.wire.turns.current_turn_origin = None;
                    self.wire.turns.current_turn_origin_from_intent = false;
                    self.recompute_phase();
                }
            }
            "async_task_state_update" => {
                let task_id = raw_update.get("asyncTaskId").and_then(Value::as_str);
                let state = raw_update.get("state").and_then(Value::as_str);
                if let (Some(task_id), Some(state)) = (task_id, state)
                    && matches!(state, "completed" | "failed" | "stopped")
                    && self.queue_state.queue.on_agent_turn_finished(task_id)
                {
                    self.flush_streams(record.seq, delta);
                    self.state.cancel_requested = false;
                    let origin = self.wire.turns.current_turn_origin.take();
                    self.wire.turns.current_turn_origin_from_intent = false;
                    self.push_turn_boundary(record, true, origin, Some(state.to_owned()), None, delta);
                    self.recompute_phase();
                }
            }
            _ => {}
        }
    }

    fn push_message_chunk(
        &mut self,
        record: &WireRecord,
        text: &str,
        message_id: Option<String>,
        delta: &mut ReduceDelta,
    ) {
        let message_id_changed = match (&self.items.streaming.message, &message_id) {
            (Some(active), Some(incoming)) => active.meta.message_id.as_deref().is_some_and(|prev| prev != incoming),
            _ => false,
        };
        if message_id_changed {
            self.flush_message(record.seq, delta);
        }

        let active = self.items.streaming.message.get_or_insert_with(|| ActiveStream {
            meta: StreamMeta {
                item_id: format!("msg-{}", record.seq),
                started_seq: record.seq,
                updated_seq: record.seq,
                ts_ms: record.ts_ms,
                message_id: message_id.clone(),
            },
            buffer: StreamingRedactor::new(self.redactor.clone()),
        });
        active.meta.updated_seq = record.seq;
        if active.meta.message_id.is_none() {
            active.meta.message_id = message_id;
        }
        let (offset, chunk_delta) = active.buffer.push(text);
        if !chunk_delta.is_empty() {
            delta.message_delta = Some(CoordinatorChatMessageDelta {
                item_id: active.meta.item_id.clone(),
                seq: record.seq,
                offset,
                text: chunk_delta,
            });
        }
    }

    fn push_thought_chunk(&mut self, record: &WireRecord, text: &str, message_id: Option<String>) {
        let active = self.items.streaming.thought.get_or_insert_with(|| ActiveStream {
            meta: StreamMeta {
                item_id: format!("thought-{}", record.seq),
                started_seq: record.seq,
                updated_seq: record.seq,
                ts_ms: record.ts_ms,
                message_id,
            },
            buffer: StreamingRedactor::new(self.redactor.clone()),
        });
        active.meta.updated_seq = record.seq;
        let _ = active.buffer.push(text);
    }

    fn flush_streams(&mut self, seq: u64, delta: &mut ReduceDelta) {
        self.flush_thought(seq, delta);
        self.flush_message(seq, delta);
    }

    fn flush_message(&mut self, seq: u64, delta: &mut ReduceDelta) {
        let Some(active) = self.items.streaming.message.take() else {
            return;
        };
        let (_offset, _flushed, text) = active.buffer.finish();
        if text.is_empty() {
            return;
        }
        let item_id = active.meta.item_id;
        self.insert_item(CoordinatorChatItem {
            id: item_id.clone(),
            seq: active.meta.started_seq,
            updated_seq: seq.max(active.meta.updated_seq),
            ts_ms: active.meta.ts_ms,
            kind: CoordinatorChatItemKind::Message(CoordinatorChatMessageItem {
                role: CoordinatorChatMessageRole::Coordinator,
                text,
                client_message_id: None,
                message_id: active.meta.message_id,
                sender: None,
            }),
        });
        delta.upserted_item_ids.push(item_id);
    }

    fn flush_thought(&mut self, seq: u64, delta: &mut ReduceDelta) {
        let Some(active) = self.items.streaming.thought.take() else {
            return;
        };
        let (_offset, _flushed, text) = active.buffer.finish();
        if text.is_empty() {
            return;
        }
        let item_id = active.meta.item_id;
        self.insert_item(CoordinatorChatItem {
            id: item_id.clone(),
            seq: active.meta.started_seq,
            updated_seq: seq.max(active.meta.updated_seq),
            ts_ms: active.meta.ts_ms,
            kind: CoordinatorChatItemKind::Thought(CoordinatorChatThoughtItem { text }),
        });
        delta.upserted_item_ids.push(item_id);
    }

    fn handle_tool_call(&mut self, record: &WireRecord, call: &ToolCall, raw_update: &Value, delta: &mut ReduceDelta) {
        let tool_call_id = call.tool_call_id.to_string();
        if self.items.indices.tool.contains_key(&tool_call_id)
            || self.items.indices.compaction.contains_key(&tool_call_id)
        {
            self.handle_tool_call_update(record, &ToolCallUpdate::from(call.clone()), raw_update, delta);
            return;
        }
        let meta = raw_update.get("_meta").unwrap_or(&Value::Null);
        let parent_tool_call_id = meta
            .pointer("/claudeCode/parentToolUseId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // A background child's call interleaves with the coordinator's own
        // streaming and does not end the open message. Finalizing here would
        // flush the redaction holdback raw and split one message in two.
        if parent_tool_call_id.is_none() {
            self.flush_streams(record.seq, delta);
        }

        let tool_name = call
            .name
            .as_deref()
            .or_else(|| meta.pointer("/claudeCode/toolName").and_then(Value::as_str))
            .map(|name| self.redactor.redact_str(name));

        // Spike finding: compaction arrives as a tool call carrying
        // `_meta.jetbrains.air.contextCompaction` (or `toolName: "compact"`),
        // not as one of the schema's compaction update kinds.
        if is_compaction_tool_call(tool_name.as_deref(), meta) {
            let notice = build_compaction_notice(
                &self.redactor,
                Some(call.title.as_str()),
                call.raw_output.as_ref(),
                meta,
                None,
            );
            let item_id = format!("notice-{}", record.seq);
            let idx = self.insert_item(CoordinatorChatItem {
                id: item_id.clone(),
                seq: record.seq,
                updated_seq: record.seq,
                ts_ms: record.ts_ms,
                kind: CoordinatorChatItemKind::Notice(notice),
            });
            self.items.indices.compaction.insert(tool_call_id, idx);
            delta.upserted_item_ids.push(item_id);
            return;
        }

        let input = call
            .raw_input
            .as_ref()
            .filter(|value| !value.is_null())
            .map(|value| self.redact_and_bound(&compact_json(value)));
        let output =
            extract_tool_output(&call.content, call.raw_output.as_ref()).map(|text| self.redact_and_bound(&text));
        let non_execution_kind = meta
            .pointer("/claudeCode/nonExecutionKind")
            .and_then(Value::as_str)
            .map(|kind| self.redactor.redact_str(kind));
        let blocked_by_auto_mode = is_auto_mode_denial(non_execution_kind.as_deref());

        let item_id = format!("tool-{}", record.seq);
        let idx = self.insert_item(CoordinatorChatItem {
            id: item_id.clone(),
            seq: record.seq,
            updated_seq: record.seq,
            ts_ms: record.ts_ms,
            kind: CoordinatorChatItemKind::ToolCall(
                CoordinatorChatToolCallItem::builder()
                    .tool_call_id(tool_call_id.clone())
                    .title(self.redactor.redact_str(&call.title))
                    .tool_kind(map_tool_kind(call.kind))
                    .status(map_tool_status(call.status))
                    .blocked_by_auto_mode(blocked_by_auto_mode)
                    .maybe_tool_name(tool_name)
                    .maybe_input(input)
                    .maybe_output(output)
                    .maybe_parent_tool_call_id(parent_tool_call_id)
                    .maybe_non_execution_kind(non_execution_kind)
                    .build(),
            ),
        });
        self.items.indices.tool.insert(tool_call_id, idx);
        delta.upserted_item_ids.push(item_id);
    }

    fn handle_tool_call_update(
        &mut self,
        record: &WireRecord,
        update: &ToolCallUpdate,
        raw_update: &Value,
        delta: &mut ReduceDelta,
    ) {
        let tool_call_id = update.tool_call_id.to_string();
        let meta = raw_update.get("_meta").unwrap_or(&Value::Null);

        if let Some(&idx) = self.items.indices.compaction.get(&tool_call_id) {
            if let Some(item) = self.items.items.get_mut(idx)
                && let CoordinatorChatItemKind::Notice(notice) = &mut item.kind
            {
                let updated = build_compaction_notice(
                    &self.redactor,
                    update.fields.title.as_deref(),
                    update.fields.raw_output.as_ref(),
                    meta,
                    Some(notice),
                );
                *notice = updated;
                item.updated_seq = record.seq;
                delta.upserted_item_ids.push(item.id.clone());
            }
            return;
        }

        let tool_name = update
            .fields
            .name
            .as_deref()
            .or_else(|| meta.pointer("/claudeCode/toolName").and_then(Value::as_str))
            .map(|name| self.redactor.redact_str(name));

        if let Some(&idx) = self.items.indices.tool.get(&tool_call_id) {
            let input = update
                .fields
                .raw_input
                .as_ref()
                .filter(|value| !value.is_null())
                .map(|value| self.redact_and_bound(&compact_json(value)));
            let output = update
                .fields
                .content
                .as_deref()
                .and_then(|content| extract_tool_output(content, update.fields.raw_output.as_ref()))
                .or_else(|| {
                    update
                        .fields
                        .raw_output
                        .as_ref()
                        .filter(|value| !value.is_null())
                        .map(compact_json)
                })
                .map(|text| self.redact_and_bound(&text));
            let title = update
                .fields
                .title
                .as_deref()
                .map(|title| self.redactor.redact_str(title));
            let parent_tool_call_id = meta
                .pointer("/claudeCode/parentToolUseId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let non_execution_kind = meta
                .pointer("/claudeCode/nonExecutionKind")
                .and_then(Value::as_str)
                .map(|kind| self.redactor.redact_str(kind));

            if let Some(item) = self.items.items.get_mut(idx)
                && let CoordinatorChatItemKind::ToolCall(tool) = &mut item.kind
            {
                if let Some(title) = title {
                    tool.title = title;
                }
                if let Some(tool_name) = tool_name {
                    tool.tool_name = Some(tool_name);
                }
                if let Some(kind) = update.fields.kind {
                    tool.tool_kind = map_tool_kind(kind);
                }
                if let Some(status) = update.fields.status {
                    tool.status = map_tool_status(status);
                }
                if let Some(input) = input {
                    tool.input = Some(input);
                }
                if let Some(output) = output {
                    tool.output = Some(output);
                }
                if let Some(parent) = parent_tool_call_id {
                    tool.parent_tool_call_id = Some(parent);
                }
                if let Some(non_exec) = non_execution_kind {
                    if is_auto_mode_denial(Some(non_exec.as_str())) {
                        tool.blocked_by_auto_mode = true;
                    }
                    tool.non_execution_kind = Some(non_exec);
                }
                item.updated_seq = record.seq;
                delta.upserted_item_ids.push(item.id.clone());
            }
            return;
        }

        // Orphan update with no preceding `tool_call`: synthesize a row.
        // Never finalize the coordinator's open message or thought for a
        // background child's orphan update.
        let parent_tool_call_id = meta
            .pointer("/claudeCode/parentToolUseId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if parent_tool_call_id.is_none() {
            self.flush_streams(record.seq, delta);
        }
        if is_compaction_tool_call(tool_name.as_deref(), meta) {
            let notice = build_compaction_notice(
                &self.redactor,
                update.fields.title.as_deref(),
                update.fields.raw_output.as_ref(),
                meta,
                None,
            );
            let item_id = format!("notice-{}", record.seq);
            let idx = self.insert_item(CoordinatorChatItem {
                id: item_id.clone(),
                seq: record.seq,
                updated_seq: record.seq,
                ts_ms: record.ts_ms,
                kind: CoordinatorChatItemKind::Notice(notice),
            });
            self.items.indices.compaction.insert(tool_call_id, idx);
            delta.upserted_item_ids.push(item_id);
            return;
        }

        let input = update
            .fields
            .raw_input
            .as_ref()
            .filter(|value| !value.is_null())
            .map(|value| self.redact_and_bound(&compact_json(value)));
        let output = update
            .fields
            .content
            .as_deref()
            .and_then(|content| extract_tool_output(content, update.fields.raw_output.as_ref()))
            .or_else(|| {
                update
                    .fields
                    .raw_output
                    .as_ref()
                    .filter(|value| !value.is_null())
                    .map(compact_json)
            })
            .map(|text| self.redact_and_bound(&text));
        let non_execution_kind = meta
            .pointer("/claudeCode/nonExecutionKind")
            .and_then(Value::as_str)
            .map(|kind| self.redactor.redact_str(kind));
        let blocked_by_auto_mode = is_auto_mode_denial(non_execution_kind.as_deref());

        let item_id = format!("tool-{}", record.seq);
        let idx = self.insert_item(CoordinatorChatItem {
            id: item_id.clone(),
            seq: record.seq,
            updated_seq: record.seq,
            ts_ms: record.ts_ms,
            kind: CoordinatorChatItemKind::ToolCall(
                CoordinatorChatToolCallItem::builder()
                    .tool_call_id(tool_call_id.clone())
                    .title(
                        self.redactor
                            .redact_str(update.fields.title.as_deref().unwrap_or("Tool call")),
                    )
                    .tool_kind(update.fields.kind.map(map_tool_kind).unwrap_or_default())
                    .status(update.fields.status.map(map_tool_status).unwrap_or_default())
                    .blocked_by_auto_mode(blocked_by_auto_mode)
                    .maybe_tool_name(tool_name)
                    .maybe_input(input)
                    .maybe_output(output)
                    .maybe_parent_tool_call_id(parent_tool_call_id)
                    .maybe_non_execution_kind(non_execution_kind)
                    .build(),
            ),
        });
        self.items.indices.tool.insert(tool_call_id, idx);
        delta.upserted_item_ids.push(item_id);
    }

    fn handle_usage_update(
        &mut self,
        record: &WireRecord,
        usage: &UsageUpdate,
        raw_update: &Value,
        delta: &mut ReduceDelta,
    ) {
        let meta = raw_update.get("_meta").unwrap_or(&Value::Null);
        let model = meta
            .get("_claude/model")
            .and_then(Value::as_str)
            .map(|value| self.redactor.redact_str(value));
        if let Some(model) = &model {
            self.state.model = Some(model.clone());
        }

        let summary = CoordinatorChatUsage {
            used: usage.used,
            size: usage.size,
            cost_amount: usage.cost.as_ref().map(|cost| cost.amount),
            cost_currency: usage.cost.as_ref().map(|cost| self.redactor.redact_str(&cost.currency)),
            model,
        };
        self.state.usage = Some(summary.clone());

        if let Some(origin_kind) = meta.pointer("/_claude~1origin/kind").and_then(Value::as_str) {
            let origin = map_turn_origin(origin_kind);
            if self.has_active_turn() {
                if !self.wire.turns.current_turn_origin_from_intent {
                    self.wire.turns.current_turn_origin = Some(origin);
                }
            } else if let Some(idx) = self.wire.turns.last_turn_boundary_idx
                && let Some(item) = self.items.items.get_mut(idx)
                && let CoordinatorChatItemKind::TurnBoundary(boundary) = &mut item.kind
                && boundary.origin.is_none()
            {
                boundary.origin = Some(origin);
                item.updated_seq = record.seq;
                delta.upserted_item_ids.push(item.id.clone());
            }
        }

        // Do not split an active streaming message/thought with a Usage item,
        // and keep `items` monotonically ordered by creation `seq`.
        let is_streaming = self.items.streaming.message.is_some() || self.items.streaming.thought.is_some();
        if !is_streaming && (summary.size > 0 || summary.used > 0) {
            let item_id = format!("usage-{}", record.seq);
            self.insert_item(CoordinatorChatItem {
                id: item_id.clone(),
                seq: record.seq,
                updated_seq: record.seq,
                ts_ms: record.ts_ms,
                kind: CoordinatorChatItemKind::Usage(summary),
            });
            delta.upserted_item_ids.push(item_id);
        }
    }

    fn push_notice(
        &mut self,
        record: &WireRecord,
        notice_kind: CoordinatorChatNoticeKind,
        text: String,
        delta: &mut ReduceDelta,
    ) {
        let item_id = format!("notice-{}", record.seq);
        self.insert_item(CoordinatorChatItem {
            id: item_id.clone(),
            seq: record.seq,
            updated_seq: record.seq,
            ts_ms: record.ts_ms,
            kind: CoordinatorChatItemKind::Notice(CoordinatorChatNoticeItem {
                notice_kind,
                text: self.redactor.redact_str(&text),
                pre_tokens: None,
                post_tokens: None,
                trigger: None,
            }),
        });
        delta.upserted_item_ids.push(item_id);
    }

    fn push_turn_boundary(
        &mut self,
        record: &WireRecord,
        agent_initiated: bool,
        origin: Option<CoordinatorChatTurnOrigin>,
        stop_reason: Option<String>,
        error: Option<String>,
        delta: &mut ReduceDelta,
    ) {
        let item_id = format!("turn-{}", record.seq);
        let idx = self.insert_item(CoordinatorChatItem {
            id: item_id.clone(),
            seq: record.seq,
            updated_seq: record.seq,
            ts_ms: record.ts_ms,
            kind: CoordinatorChatItemKind::TurnBoundary(CoordinatorChatTurnBoundaryItem {
                agent_initiated,
                origin,
                stop_reason: self.redactor.redact_opt_str(stop_reason),
                error: self.redactor.redact_opt_str(error),
            }),
        });
        self.wire.turns.last_turn_boundary_idx = Some(idx);
        delta.upserted_item_ids.push(item_id);
    }

    fn lookup_permission_option_name(&self, display_id: &str, option_id: &str) -> Option<String> {
        let &idx = self.items.indices.permission.get(display_id)?;
        let item = self.items.items.get(idx)?;
        let CoordinatorChatItemKind::PermissionRequest(permission) = &item.kind else {
            return None;
        };
        permission
            .options
            .iter()
            .find(|option| option.option_id == option_id)
            .map(|option| option.name.clone())
    }

    fn resolve_permission_item(
        &mut self,
        display_id: &str,
        resolution: CoordinatorChatPermissionResolution,
        seq: u64,
        delta: &mut ReduceDelta,
    ) {
        let Some(&idx) = self.items.indices.permission.get(display_id) else {
            return;
        };
        let Some(item) = self.items.items.get_mut(idx) else {
            return;
        };
        if let CoordinatorChatItemKind::PermissionRequest(permission) = &mut item.kind
            && permission.resolution == CoordinatorChatPermissionResolution::Pending
        {
            permission.resolution = resolution;
            item.updated_seq = seq;
            delta.upserted_item_ids.push(item.id.clone());
        }
    }

    fn has_active_turn(&self) -> bool {
        self.has_prompt_in_flight() || !self.queue_state.queue.open_agent_turns().is_empty()
    }

    fn has_prompt_in_flight(&self) -> bool {
        self.wire
            .pending_client_requests
            .values()
            .any(|pending| pending.method == "session/prompt")
    }

    fn recompute_phase(&mut self) {
        if matches!(self.state.phase, CoordinatorChatConnectionPhase::Failed { .. }) {
            return;
        }
        if !self.wire.pending_agent_permissions.is_empty() {
            self.state.phase = CoordinatorChatConnectionPhase::WaitingForPermission;
        } else if self.has_active_turn() {
            self.state.phase = CoordinatorChatConnectionPhase::Working;
        } else if self.state.session_id.is_some() {
            self.state.phase = CoordinatorChatConnectionPhase::Idle;
        } else if self.state.phase != CoordinatorChatConnectionPhase::Initializing {
            self.state.phase = CoordinatorChatConnectionPhase::Starting;
        }
    }

    /// Redacts BEFORE bounding so a credential straddling the truncation
    /// boundary cannot leave a partial secret prefix behind.
    fn redact_and_bound(&self, text: &str) -> String {
        let redacted = self.redactor.redact_str(text);
        bound_text(&redacted, self.items.tool_text_limit)
    }

    /// Captures a versioned snapshot of the reducer's full state at the
    /// current `(last_wire_seq, last_intent_seq)` boundary.
    pub fn snapshot(&self) -> crate::snapshot::ReducerSnapshot {
        crate::snapshot::ReducerSnapshot {
            schema_version: crate::snapshot::SNAPSHOT_SCHEMA_VERSION,
            cursor: crate::snapshot::SnapshotCursor {
                wire_seq: self.state.last_seq,
                intent_seq: self.queue_state.last_intent_seq,
                tool_text_limit: self.items.tool_text_limit,
            },
            projection: crate::snapshot::SnapshotProjection {
                state: self.state.clone(),
                items: self.items.items.clone(),
                compaction_indices: self.items.indices.compaction.clone(),
                streaming_message: self
                    .items
                    .streaming
                    .message
                    .as_ref()
                    .map(|active| ActiveStreamSnapshot {
                        meta: active.meta.clone(),
                        buffer: active.buffer.snapshot(),
                    }),
                streaming_thought: self
                    .items
                    .streaming
                    .thought
                    .as_ref()
                    .map(|active| ActiveStreamSnapshot {
                        meta: active.meta.clone(),
                        buffer: active.buffer.snapshot(),
                    }),
            },
            wire: self.wire.clone(),
            queue: self.queue_state.queue.clone(),
        }
    }

    /// Restores a reducer from a validated [`crate::snapshot::ReducerSnapshot`]
    /// and re-attaches `redactor`.
    ///
    /// Returns an error if the snapshot version mismatches or any internal
    /// item index is out of bounds or points to the wrong item variant.
    pub fn from_snapshot(
        snapshot: crate::snapshot::ReducerSnapshot,
        redactor: CredentialRedactor,
    ) -> Result<Self, String> {
        if snapshot.schema_version != crate::snapshot::SNAPSHOT_SCHEMA_VERSION {
            return Err(format!(
                "snapshot schema version {} does not match expected {}",
                snapshot.schema_version,
                crate::snapshot::SNAPSHOT_SCHEMA_VERSION
            ));
        }
        if snapshot.cursor.wire_seq != snapshot.projection.state.last_seq {
            return Err(format!(
                "snapshot wire_seq {} disagrees with state.last_seq {}",
                snapshot.cursor.wire_seq, snapshot.projection.state.last_seq
            ));
        }
        if let Some(last_idx) = snapshot.wire.turns.last_turn_boundary_idx {
            match snapshot.projection.items.get(last_idx) {
                Some(item) if matches!(item.kind, CoordinatorChatItemKind::TurnBoundary(_)) => {}
                _ => {
                    return Err(format!(
                        "snapshot last_turn_boundary_idx {last_idx} does not point to a TurnBoundary item"
                    ));
                }
            }
        }
        for (tool_call_id, &idx) in &snapshot.projection.compaction_indices {
            match snapshot.projection.items.get(idx) {
                Some(item)
                    if matches!(
                        &item.kind,
                        CoordinatorChatItemKind::Notice(notice)
                            if notice.notice_kind == CoordinatorChatNoticeKind::Compaction
                    ) => {}
                _ => {
                    return Err(format!(
                        "snapshot compaction index {idx} for {tool_call_id} does not point to a Compaction notice"
                    ));
                }
            }
        }

        let mut tool = HashMap::new();
        let mut permission = HashMap::new();
        for (idx, item) in snapshot.projection.items.iter().enumerate() {
            match &item.kind {
                CoordinatorChatItemKind::ToolCall(t) => {
                    tool.insert(t.tool_call_id.clone(), idx);
                }
                CoordinatorChatItemKind::PermissionRequest(perm) => {
                    permission.insert(perm.request_id.clone(), idx);
                }
                _ => {}
            }
        }

        let message = snapshot.projection.streaming_message.map(|active| ActiveStream {
            meta: active.meta,
            buffer: StreamingRedactor::from_snapshot(redactor.clone(), active.buffer),
        });
        let thought = snapshot.projection.streaming_thought.map(|active| ActiveStream {
            meta: active.meta,
            buffer: StreamingRedactor::from_snapshot(redactor.clone(), active.buffer),
        });

        Ok(Self {
            redactor,
            state: snapshot.projection.state,
            items: ItemStore {
                items: snapshot.projection.items,
                indices: ItemIndices {
                    tool,
                    compaction: snapshot.projection.compaction_indices,
                    permission,
                },
                streaming: StreamingBuffers { message, thought },
                tool_text_limit: snapshot.cursor.tool_text_limit,
            },
            wire: snapshot.wire,
            queue_state: QueueTracker {
                queue: snapshot.queue,
                last_intent_seq: snapshot.cursor.intent_seq,
            },
        })
    }

    fn format_rpc_error(&self, prefix: &str, error: &Value) -> String {
        let code = error.get("code").and_then(Value::as_i64);
        let message = error.get("message").and_then(Value::as_str).unwrap_or("unknown error");
        let raw = match code {
            Some(code) => format!("{prefix}: {message} (code {code})"),
            None => format!("{prefix}: {message}"),
        };
        self.redactor.redact_str(&raw)
    }
}

/// Replays a slice or iterator of [`WireRecord`]s through a fresh reducer.
pub fn replay_records<'a>(
    records: impl IntoIterator<Item = &'a WireRecord>,
    redactor: CredentialRedactor,
) -> ConversationReducer {
    let mut reducer = ConversationReducer::new(redactor);
    for record in records {
        reducer.apply_record(record);
    }
    reducer
}

/// Replays a wire journal joined with an append-only intent log through a
/// fresh reducer, recovering both conversation items (with prompt origins and
/// redacted prompt text) and the turn queue state.
///
/// Intent records are indexed before wire records so each `ToAgent`
/// `session/prompt` joins with its matching `IntentRecord::Sent` by
/// `request_id` (FIFO per id). After all wire records have been applied, any
/// `Sent` intent that never appeared on the wire journal is returned to the
/// queue via [`ConversationReducer::reconcile_unsent_dispatches`]; only use
/// this helper on complete/offline logs that already reach the host's live
/// boundary.
pub fn replay_with_intent_log<'a>(
    records: impl IntoIterator<Item = &'a WireRecord>,
    intent_records: impl IntoIterator<Item = &'a IntentRecord>,
    redactor: CredentialRedactor,
    tool_text_limit: usize,
) -> ConversationReducer {
    let mut reducer = ConversationReducer::with_tool_text_limit(redactor, tool_text_limit);
    for intent in intent_records {
        reducer.apply_intent_record(intent);
    }
    for record in records {
        reducer.apply_record(record);
    }
    reducer.reconcile_unsent_dispatches();
    reducer
}

/// Reads a wire journal from `reader` (tolerating a torn final record via
/// [`boss_agent_host::journal::read_records`]) and replays it through a fresh
/// reducer.
pub fn replay_reader(reader: impl BufRead, redactor: CredentialRedactor) -> io::Result<ConversationReducer> {
    let records = read_records(reader)?;
    Ok(replay_records(&records, redactor))
}

fn parse_request_id(value: Option<&Value>) -> Option<RequestId> {
    let value = value?;
    if value.is_null() {
        return None;
    }
    serde_json::from_value::<RequestId>(value.clone()).ok()
}

/// Canonical map key for a JSON-RPC `RequestId` that distinguishes numeric
/// ids (`1`) from string ids (`"1"`).
fn request_id_key(id: &RequestId) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| id.to_string())
}

fn compact_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn bound_text(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        None => text.to_owned(),
        Some((cut, _)) => {
            let kept = &text[..cut];
            // Tool output arrives wrapped in a markdown fence; close it so the
            // notice does not land inside an unterminated code block.
            let fence_open = kept.lines().filter(|line| line.trim_start().starts_with("```")).count() % 2 == 1;
            format!(
                "{kept}{}\n… truncated: {limit} of {} characters shown",
                if fence_open { "\n```" } else { "" },
                text.chars().count()
            )
        }
    }
}

fn extract_tool_output(content: &[ToolCallContent], raw_output: Option<&Value>) -> Option<String> {
    let mut text = String::new();
    for block in content {
        let piece = match block {
            ToolCallContent::Content(content) => match &content.content {
                ContentBlock::Text(text_block) => Some(text_block.text.clone()),
                _ => None,
            },
            ToolCallContent::Diff(diff) => Some(format!("[diff] {}", diff.path.display())),
            ToolCallContent::Terminal(_) => Some("[terminal]".to_owned()),
            ToolCallContent::Session(session) => Some(format!("[session] {}", session.session_id)),
            _ => None,
        };
        if let Some(piece) = piece {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&piece);
        }
    }
    if text.is_empty() {
        text = raw_output.filter(|output| !output.is_null()).map(compact_json)?;
    }
    Some(text)
}

fn is_compaction_tool_call(tool_name: Option<&str>, meta: &Value) -> bool {
    meta.pointer("/jetbrains/air/contextCompaction").is_some()
        || tool_name.is_some_and(|name| name.eq_ignore_ascii_case("compact"))
}

fn build_compaction_notice(
    redactor: &CredentialRedactor,
    title: Option<&str>,
    raw_output: Option<&Value>,
    meta: &Value,
    existing: Option<&CoordinatorChatNoticeItem>,
) -> CoordinatorChatNoticeItem {
    let stats = meta.pointer("/jetbrains/air/contextCompaction").or(raw_output);
    let trigger = stats
        .and_then(|output| output.get("trigger"))
        .and_then(Value::as_str)
        .map(|value| redactor.redact_str(value))
        .or_else(|| existing.and_then(|prev| prev.trigger.clone()));
    let pre_tokens = stats
        .and_then(|output| output.get("preTokens"))
        .and_then(Value::as_u64)
        .or_else(|| existing.and_then(|prev| prev.pre_tokens));
    let post_tokens = stats
        .and_then(|output| output.get("postTokens"))
        .and_then(Value::as_u64)
        .or_else(|| existing.and_then(|prev| prev.post_tokens));

    let text = match (trigger.as_deref(), pre_tokens, post_tokens) {
        (Some(trigger), Some(pre), Some(post)) => {
            format!("Context compacted ({trigger}: {pre} -> {post} tokens)")
        }
        (None, Some(pre), Some(post)) => format!("Context compacted ({pre} -> {post} tokens)"),
        _ => title
            .map(|t| redactor.redact_str(t))
            .or_else(|| existing.map(|prev| prev.text.clone()))
            .unwrap_or_else(|| "Compact conversation".to_owned()),
    };

    CoordinatorChatNoticeItem {
        notice_kind: CoordinatorChatNoticeKind::Compaction,
        text,
        pre_tokens,
        post_tokens,
        trigger,
    }
}

fn extract_model_from_config_options(options: Option<&Value>) -> Option<String> {
    let options = options?.as_array()?;
    for option in options {
        if option.get("id").and_then(Value::as_str) == Some("model")
            && let Some(value) = option.get("currentValue").and_then(Value::as_str)
        {
            return Some(value.to_owned());
        }
    }
    None
}

fn extract_model_from_typed_options(options: &[boss_acp_client::schema::SessionConfigOption]) -> Option<String> {
    for option in options {
        let Ok(Value::Object(obj)) = serde_json::to_value(option) else {
            continue;
        };
        if obj.get("id").and_then(Value::as_str) == Some("model")
            && let Some(value) = obj.get("currentValue").and_then(Value::as_str)
        {
            return Some(value.to_owned());
        }
    }
    None
}

fn map_tool_kind(kind: ToolKind) -> CoordinatorChatToolKind {
    match kind {
        ToolKind::Read => CoordinatorChatToolKind::Read,
        ToolKind::Edit => CoordinatorChatToolKind::Edit,
        ToolKind::Delete => CoordinatorChatToolKind::Delete,
        ToolKind::Move => CoordinatorChatToolKind::Move,
        ToolKind::Search => CoordinatorChatToolKind::Search,
        ToolKind::Execute => CoordinatorChatToolKind::Execute,
        ToolKind::Think => CoordinatorChatToolKind::Think,
        ToolKind::Fetch => CoordinatorChatToolKind::Fetch,
        ToolKind::SwitchMode => CoordinatorChatToolKind::SwitchMode,
        _ => CoordinatorChatToolKind::Other,
    }
}

fn map_tool_status(status: ToolCallStatus) -> CoordinatorChatToolStatus {
    match status {
        ToolCallStatus::Pending => CoordinatorChatToolStatus::Pending,
        ToolCallStatus::InProgress => CoordinatorChatToolStatus::InProgress,
        ToolCallStatus::Completed => CoordinatorChatToolStatus::Completed,
        ToolCallStatus::Failed => CoordinatorChatToolStatus::Failed,
        _ => CoordinatorChatToolStatus::Unknown,
    }
}

fn map_permission_option_kind(kind: PermissionOptionKind) -> CoordinatorChatPermissionOptionKind {
    match kind {
        PermissionOptionKind::AllowOnce => CoordinatorChatPermissionOptionKind::AllowOnce,
        PermissionOptionKind::AllowAlways => CoordinatorChatPermissionOptionKind::AllowAlways,
        PermissionOptionKind::RejectOnce => CoordinatorChatPermissionOptionKind::RejectOnce,
        PermissionOptionKind::RejectAlways => CoordinatorChatPermissionOptionKind::RejectAlways,
        _ => CoordinatorChatPermissionOptionKind::Other,
    }
}

fn map_stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        _ => "unknown",
    }
}

fn map_turn_origin(raw: &str) -> CoordinatorChatTurnOrigin {
    match raw {
        "human" => CoordinatorChatTurnOrigin::Human,
        "engine" => CoordinatorChatTurnOrigin::Engine,
        "peer" => CoordinatorChatTurnOrigin::Peer,
        "task-notification" | "task_notification" => CoordinatorChatTurnOrigin::TaskNotification,
        _ => CoordinatorChatTurnOrigin::Unknown,
    }
}

fn is_auto_mode_denial(non_execution_kind: Option<&str>) -> bool {
    let Some(kind) = non_execution_kind else {
        return false;
    };
    let lower = kind.to_ascii_lowercase();
    lower.contains("auto") || lower.contains("classifier")
}

fn is_login_required_error(code: Option<i64>, message: &str) -> bool {
    if code == Some(-32000) {
        return true;
    }
    let lower = message.to_ascii_lowercase();
    lower.contains("authentication required") || lower.contains("login required")
}

/// Spike S16: when the Claude Code child dies under a live adapter, the
/// in-flight prompt fails with `-32603` ("process exited unexpectedly") and
/// every subsequent prompt fails with `-32002` ("Resource not found").
fn is_session_fatal_error(code: Option<i64>, message: &str) -> bool {
    if code == Some(-32002) {
        return true;
    }
    if code == Some(-32603) {
        let lower = message.to_ascii_lowercase();
        return lower.contains("exited unexpectedly") || lower.contains("process died");
    }
    false
}
