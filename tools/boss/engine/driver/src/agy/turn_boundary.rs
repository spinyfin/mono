//! Parent identity is checked by the progress session before reaching here.
//! Non-idle Stops are background work, not a re-entrant boundary. If a parent
//! never emits an idle Stop (observed on 1.1.12; possible on later versions),
//! it remains active until existing liveness handling or an explicit interrupt resolves it.
//! Neither a child's Stop nor PostInvocation can recover that missing boundary.

use boss_protocol::{NormalizeError, StopReason, WorkerEvent};
use serde_json::Value;

use crate::TurnEnd;

pub(super) fn normalize_stop(raw: &Value) -> Result<Option<WorkerEvent>, NormalizeError> {
    let idle = raw
        .get("fully_idle")
        .and_then(Value::as_bool)
        .ok_or_else(|| NormalizeError::Malformed("agy Stop requires boolean fullyIdle".into()))?;
    if !idle {
        return Ok(None);
    }
    let reason = raw.get("termination_reason").and_then(Value::as_str);
    let stop_reason = match reason {
        // The only values observed on interactive hooks in the studies.
        Some("NO_TOOL_CALL" | "TERMINAL_CUSTOM_HOOK") => StopReason::Completed,
        // In the binary enum, but Esc never actually emitted this hook.
        Some("USER_CANCELED") => StopReason::Interrupted,
        Some(
            "UNSPECIFIED"
            | "ERROR"
            | "MAX_INVOCATIONS"
            | "MAX_FORCED_INVOCATIONS"
            | "EARLY_CONTINUE"
            | "TERMINAL_STEP_TYPE"
            | "INJECTED_RESPONSE"
            | "MAX_TOKEN_BUDGET_EXCEEDED"
            | "HALTED_STEP"
            | "CHAINED",
        ) => StopReason::Other,
        Some(reason) => {
            // Retain the exact unknown spelling for diagnostics, never the prompt.
            tracing::warn!(
                termination_reason = reason,
                fully_idle = idle,
                "agy Stop has an unknown termination reason"
            );
            StopReason::Other
        }
        None => {
            tracing::warn!(
                fully_idle = idle,
                "agy Stop has a missing or non-string termination reason"
            );
            StopReason::Other
        }
    };
    let session_id = raw
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(NormalizeError::MissingField("session_id"))?;
    Ok(Some(WorkerEvent::Stop {
        session_id: session_id.to_owned(),
        stop_hook_active: false,
        stop_reason,
    }))
}

pub(super) fn turn_boundary(event: &WorkerEvent) -> Option<TurnEnd> {
    match event {
        WorkerEvent::Stop {
            session_id,
            stop_reason,
            ..
        } => Some(TurnEnd {
            session_id: session_id.clone(),
            reason: *stop_reason,
            // fullyIdle=false never reaches here; invocationNum resets on
            // automatic re-invocation. Claude's stop_hook_active is unrelated.
            continuation: false,
        }),
        _ => None,
    }
}
