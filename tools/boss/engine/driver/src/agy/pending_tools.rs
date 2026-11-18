//! Correlate tool calls across hook connections, by (conversation, id) rather than args.
//!
//! Calls from the claimed parent and from native child conversations are all
//! tracked; a `PostToolUse` closes only the call with the same conversation
//! and id, so a child's event can never close the parent's call. Unmatched
//! calls from every conversation are flagged at the claimed parent's idle
//! `Stop`.
//!
//! The snapshot lives in the in-memory progress-session state of the events
//! socket (and is restored from it per hook connection). It is not persisted
//! across engine restarts: a restart between a refused `PreToolUse` and the
//! parent's idle `Stop` loses the pending calls, so a missed refusal in that
//! window is possible and this detector must not be assumed complete.

use std::collections::BTreeMap;

use boss_protocol::WorkerEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifies Antigravity evidence inside the shared unobserved-command marker.
pub const REFUSED_TOOL_PREFIX: &str = "agy refused/unobserved tool: ";

#[derive(Default, Serialize, Deserialize)]
pub(super) struct PendingTools {
    /// Keyed by `conversation/toolCall.id`.
    calls: BTreeMap<String, String>,
    #[serde(default)]
    missing_id: bool,
}

fn call_key(conversation: &str, id: &str) -> String {
    format!("{conversation}/{id}")
}

impl PendingTools {
    pub(super) fn observe(&mut self, raw: &Value, event: &WorkerEvent, parent: Option<&str>) {
        let (session_id, starting) = match event {
            WorkerEvent::PreToolUse { session_id, .. } => (session_id, true),
            WorkerEvent::PostToolUse { session_id, .. } => (session_id, false),
            _ => return,
        };
        if parent.is_none() {
            return;
        }
        let Some(id) = raw
            .pointer("/toolCall/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            self.missing_id |= starting;
            return;
        };
        if starting {
            let name = raw
                .pointer("/toolCall/name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            self.calls.entry(call_key(session_id, id)).or_insert_with(|| {
                // Do not retain the prompt or arbitrary tool arguments in the audit trail.
                format!("{REFUSED_TOOL_PREFIX}conversation={session_id}, call={id}, tool={name}")
            });
        } else {
            self.calls.remove(&call_key(session_id, id));
        }
    }

    pub(super) fn drain(&mut self, session_id: &str) -> Vec<WorkerEvent> {
        let mut calls: Vec<_> = std::mem::take(&mut self.calls).into_values().collect();
        if std::mem::take(&mut self.missing_id) {
            calls.push(format!(
                "{REFUSED_TOOL_PREFIX}conversation={session_id}, missing toolCall.id; completion cannot be correlated"
            ));
        }
        calls
            .into_iter()
            .map(|call| WorkerEvent::Notification {
                session_id: session_id.to_owned(),
                message: crate::codex::unobserved_command_notification(&call),
            })
            .collect()
    }
}
