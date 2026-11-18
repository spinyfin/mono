//! Correlate parent tool calls across hook connections, by id rather than args.

use std::collections::BTreeMap;

use boss_protocol::WorkerEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifies Antigravity evidence inside the shared unobserved-command marker.
pub const REFUSED_TOOL_PREFIX: &str = "agy refused/unobserved tool: ";

#[derive(Default, Serialize, Deserialize)]
pub(super) struct PendingTools {
    calls: BTreeMap<String, String>,
    #[serde(default)]
    missing_id: bool,
}

impl PendingTools {
    pub(super) fn observe(&mut self, raw: &Value, event: &WorkerEvent, parent: Option<&str>) {
        let (session_id, starting) = match event {
            WorkerEvent::PreToolUse { session_id, .. } => (session_id, true),
            WorkerEvent::PostToolUse { session_id, .. } => (session_id, false),
            _ => return,
        };
        if parent != Some(session_id.as_str()) {
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
            self.calls.entry(id.to_owned()).or_insert_with(|| {
                // Do not retain the prompt or arbitrary tool arguments in the audit trail.
                format!("{REFUSED_TOOL_PREFIX}conversation={session_id}, call={id}, tool={name}")
            });
        } else {
            self.calls.remove(id);
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
