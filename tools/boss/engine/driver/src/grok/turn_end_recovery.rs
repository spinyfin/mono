//! Turn-end recovery for Esc-cancelled Grok turns (design T-12).
//!
//! Esc-cancelled Grok turns skip the `Stop` hook entirely — the cancellation
//! is only ever recorded in `$GROK_HOME/sessions/<pct-encoded-cwd>/<sid>/events.jsonl`
//! as a `turn_ended` record with `outcome: "cancelled"`. Without a recovery
//! path the engine's turn boundary for this driver (`GrokDriver::turn_boundary`,
//! which only fires on `WorkerEvent::Stop`) never fires for an interrupted
//! turn, and the slot pins at `Working` forever while the worker actually
//! sits idle at its prompt.
//!
//! This module supplies the two pieces [`crate::AgentDriver::prepare_interrupt_recovery`]
//! / [`crate::AgentDriver::is_interrupt_recovery_turn_end`] need: constructing
//! the run's `events.jsonl` path (fully derivable from `GROK_HOME`, the
//! workspace `--cwd`, and the Boss-assigned session UUID — no glob, no
//! correlation step) and recognising a cancelled-turn-end record. The bounded
//! tail-with-fallback loop itself is engine-owned (`core/src/interrupt_recovery.rs`)
//! so this crate stays free of a tokio runtime dependency; this module is
//! deliberately synchronous and side-effect-free.
//!
//! Path-encoding scheme confirmed empirically against real
//! `$GROK_HOME/sessions/` directories on a host with `grok` installed:
//! `/` becomes `%2F`, a space becomes `%20`, and unreserved characters
//! (letters, digits, `-`, `.`, `_`, `~`) are left alone — standard RFC 3986
//! percent-encoding of the whole absolute path as one opaque segment.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::home::{grok_home_for_run, read_session_id, read_workspace_path_stamp};
use crate::InterruptRecoverySnapshot;

/// How long the engine waits for a cancellation record to appear before
/// falling back to a synthesized turn end. Generous relative to the
/// sub-second latency observed between Esc and the `turn_ended` record
/// landing (`ghostty-grok-pane-viability-artifacts`), but bounded so a
/// genuinely wedged interrupt doesn't pin the slot invisibly for long.
pub const SETTLE_WINDOW: Duration = Duration::from_secs(8);

/// Percent-encode `s` as one opaque path segment: unreserved characters
/// (`A-Z a-z 0-9 - _ . ~`) pass through; everything else (including `/`)
/// becomes an uppercase `%XX` escape of its UTF-8 bytes. Matches the scheme
/// Grok uses for `$GROK_HOME/sessions/<encoded-cwd>/...`.
pub fn percent_encode_path_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Build the run's `events.jsonl` path from `grok_home`, the workspace
/// `--cwd`, and the Boss-assigned session UUID.
pub fn events_jsonl_path(grok_home: &Path, workspace: &Path, session_id: &str) -> PathBuf {
    grok_home
        .join("sessions")
        .join(percent_encode_path_component(&workspace.display().to_string()))
        .join(session_id)
        .join("events.jsonl")
}

/// Whether `raw` is a Grok `events.jsonl` cancelled-turn-end record:
/// `{"type": "turn_ended", "outcome": "cancelled", ...}`. Confirmed against
/// both the redacted spike artifact and live `events.jsonl` files on a host
/// with `grok` installed. Any other `turn_ended` outcome (`completed`,
/// `error`) or event type (`phase_changed`, `tool_started`, …) returns
/// `false` — the tail keeps reading rather than treating it as a match.
pub fn is_cancelled_turn_end(raw: &Value) -> bool {
    raw.get("type").and_then(Value::as_str) == Some("turn_ended")
        && raw.get("outcome").and_then(Value::as_str) == Some("cancelled")
}

/// Build the [`InterruptRecoverySnapshot`] for `run_id`: resolve `GROK_HOME`,
/// the stamped session id and workspace path, construct the `events.jsonl`
/// path, and snapshot its current byte length as the tail's starting offset
/// (0 when the file does not exist yet).
///
/// Returns `None` (with a debug log) rather than an error when any of the
/// per-run state this needs is missing — a run that never provisioned (or
/// whose `GROK_HOME` state has already been torn down) has nothing to
/// recover, and a missing snapshot must not block interrupt delivery itself.
pub fn prepare_snapshot(run_id: &str) -> Option<InterruptRecoverySnapshot> {
    let grok_home = match grok_home_for_run(run_id) {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!(run_id, %err, "interrupt recovery: cannot resolve GROK_HOME; skipping");
            return None;
        }
    };
    let session_id = match read_session_id(&grok_home) {
        Ok(id) => id,
        Err(err) => {
            tracing::debug!(run_id, %err, "interrupt recovery: no stamped session id; skipping");
            return None;
        }
    };
    let workspace = match read_workspace_path_stamp(&grok_home) {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!(run_id, %err, "interrupt recovery: no stamped workspace path; skipping");
            return None;
        }
    };
    let events_path = events_jsonl_path(&grok_home, &workspace, &session_id);
    let offset = std::fs::metadata(&events_path).map(|meta| meta.len()).unwrap_or(0);
    Some(InterruptRecoverySnapshot {
        events_path,
        offset,
        session_id,
        settle_window: SETTLE_WINDOW,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn percent_encodes_slash_and_space_leaves_unreserved_alone() {
        assert_eq!(
            percent_encode_path_component("/Users/brianduff/.local/share/cube/workspaces/mono-agent-001"),
            "%2FUsers%2Fbrianduff%2F.local%2Fshare%2Fcube%2Fworkspaces%2Fmono-agent-001"
        );
        assert_eq!(
            percent_encode_path_component("/Users/brianduff/Library/Application Support/Boss/diagnostics"),
            "%2FUsers%2Fbrianduff%2FLibrary%2FApplication%20Support%2FBoss%2Fdiagnostics"
        );
    }

    #[test]
    fn events_jsonl_path_matches_documented_layout() {
        let grok_home = Path::new("/tmp/grok-home-1");
        let workspace = Path::new("/Users/brianduff/.local/share/cube/workspaces/mono-agent-001");
        let session_id = "bf9b7291-f5ab-48db-9a71-3bffe7c25ea0";
        let path = events_jsonl_path(grok_home, workspace, session_id);
        assert_eq!(
            path,
            Path::new(
                "/tmp/grok-home-1/sessions/%2FUsers%2Fbrianduff%2F.local%2Fshare%2Fcube%2Fworkspaces%2Fmono-agent-001/bf9b7291-f5ab-48db-9a71-3bffe7c25ea0/events.jsonl"
            )
        );
    }

    /// Exact record from the Q8 spike artifact
    /// (`ghostty-grok-pane-viability-artifacts/.../session_telemetry_excerpt.md`),
    /// re-confirmed against live `events.jsonl` files on a host with `grok`
    /// installed.
    #[test]
    fn recognises_the_documented_cancelled_turn_end_record() {
        let raw = json!({
            "ts": "2026-07-27T23:12:52.330Z",
            "type": "turn_ended",
            "outcome": "cancelled",
            "cancellation_category": "mid_turn_abort",
            "cancellation_context": {"trigger": "esc"}
        });
        assert!(is_cancelled_turn_end(&raw));
    }

    #[test]
    fn recognises_a_minimal_cancelled_record_with_no_extra_fields() {
        // Live sessions on this host show `cancellation_context` is not
        // always present (e.g. a bare `mid_turn_abort` cancellation) —
        // the matcher must not require it.
        let raw = json!({"type": "turn_ended", "outcome": "cancelled", "cancellation_category": "mid_turn_abort"});
        assert!(is_cancelled_turn_end(&raw));
    }

    #[test]
    fn rejects_a_completed_turn_end() {
        let raw = json!({"ts": "2026-07-27T23:12:57.718Z", "type": "turn_ended", "outcome": "completed"});
        assert!(!is_cancelled_turn_end(&raw));
    }

    #[test]
    fn rejects_an_error_turn_end() {
        let raw = json!({"type": "turn_ended", "outcome": "error"});
        assert!(!is_cancelled_turn_end(&raw));
    }

    #[test]
    fn rejects_unrelated_event_types() {
        for raw in [
            json!({"type": "phase_changed"}),
            json!({"type": "tool_started", "tool_name": "run_terminal_command"}),
            json!({"type": "turn_started", "session_id": "s1", "turn_number": 0}),
            json!({}),
        ] {
            assert!(!is_cancelled_turn_end(&raw), "must reject: {raw}");
        }
    }
}
