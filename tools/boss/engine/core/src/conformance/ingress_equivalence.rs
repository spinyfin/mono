//! Ingress equivalence: stdout-JSONL and hook ingress produce the same
//! [`WorkerEvent`] sequence for an equivalent session.
//!
//! This is the core claim the driver seam makes and nothing else currently
//! tests. Both sides go through [`AgentDriver::normalize_progress_event`].

use crate::conformance::fixtures::{
    CANONICAL_SESSION_ID, CLAUDE_HOOK_SESSION_JSONL, CODEX_STDOUT_SESSION_JSONL, codex_shaped_driver, decode_jsonl,
    expected_session_events, normalize_session_id,
};
use crate::driver::ClaudeDriver;

#[test]
fn hook_ingress_matches_expected_session_sequence() {
    let events: Vec<_> = decode_jsonl(&ClaudeDriver, CLAUDE_HOOK_SESSION_JSONL)
        .into_iter()
        .map(|e| normalize_session_id(e, CANONICAL_SESSION_ID))
        .collect();
    assert_eq!(
        events,
        expected_session_events(),
        "Claude hook ingress must decode to the canonical activity-machine sequence",
    );
}

#[test]
fn stdout_jsonl_ingress_matches_expected_session_sequence() {
    let driver = codex_shaped_driver();
    let events: Vec<_> = decode_jsonl(&driver, CODEX_STDOUT_SESSION_JSONL)
        .into_iter()
        .map(|e| normalize_session_id(e, CANONICAL_SESSION_ID))
        .collect();
    assert_eq!(
        events,
        expected_session_events(),
        "Codex stdout-JSONL ingress must decode to the same canonical sequence as hooks",
    );
}

#[test]
fn stdout_jsonl_and_hook_ingress_produce_identical_worker_event_sequences() {
    let hook_events: Vec<_> = decode_jsonl(&ClaudeDriver, CLAUDE_HOOK_SESSION_JSONL)
        .into_iter()
        .map(|e| normalize_session_id(e, CANONICAL_SESSION_ID))
        .collect();
    let stdout_events: Vec<_> = decode_jsonl(&codex_shaped_driver(), CODEX_STDOUT_SESSION_JSONL)
        .into_iter()
        .map(|e| normalize_session_id(e, CANONICAL_SESSION_ID))
        .collect();
    assert_eq!(
        hook_events, stdout_events,
        "core driver-seam claim: both transports must yield the same WorkerEvent sequence",
    );
}

#[test]
fn stdout_jsonl_skips_unmappable_agent_message_without_losing_later_events() {
    // The fixture carries an `agent_message` item between the tool completion
    // and `turn.completed`. The normaliser must reject it (no activity-machine
    // analog) without dropping the subsequent Stop — tolerance, not crash.
    let events = decode_jsonl(&codex_shaped_driver(), CODEX_STDOUT_SESSION_JSONL);
    let stops = events
        .iter()
        .filter(|e| matches!(e, boss_protocol::WorkerEvent::Stop { .. }))
        .count();
    assert_eq!(
        stops, 1,
        "turn.completed must still surface as Stop after an unmappable item"
    );
    assert_eq!(events.len(), expected_session_events().len());
}
