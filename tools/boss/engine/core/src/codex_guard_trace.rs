//! Engine-side observation of Codex `PreToolUse` guard execution.
//!
//! The Codex driver wraps every guard it arms in a trace shim that records one
//! line per invocation under the run's `CODEX_HOME`
//! ([`boss_engine_driver::codex::guard_trace`]), and its rollout progress
//! session reports that trace at each turn boundary as a
//! `WorkerEvent::Notification`:
//!
//! - [`boss_engine_driver::codex::GUARD_TRACE_MARKER`] — guards ran; the
//!   message carries the counts and the reason head of every block or guard
//!   failure. This is the signal that makes "did the guard fire for this
//!   execution, and what did it decide?" answerable for a Codex run at all;
//!   nothing in Codex's own stream carries it.
//! - [`boss_engine_driver::codex::GUARDS_SILENT_MARKER`] — the turn ran tool
//!   calls and **no** guard invocation was recorded. Codex's hook failures are
//!   documented as silent and fail-open (an untrusted hook is skipped with no
//!   stream event; an unexecutable handler produces no diagnostic), so this is
//!   the only observable difference between "guardrails enforced" and
//!   "guardrails inert". Boss's worker prompt asserts pushes are blocked, so
//!   this is recorded at `error` level: it means that assertion was not being
//!   enforced.
//!
//! This module owns the counters; the dispatch that feeds them lives in
//! `app/worker_events.rs` alongside the other notification-marker handlers.

crate::register_counter!(
    CODEX_GUARD_TRACE_REPORTED,
    "codex.guard_trace.reported",
    "A Codex turn reported its PreToolUse guard activity (guards ran; counts and any blocks are \
     in the engine log line).",
);

crate::register_counter!(
    CODEX_GUARDS_SILENT,
    "codex.guard_trace.silent",
    "A Codex turn ran tool calls with no PreToolUse guard invocation recorded — the observable \
     signature of Codex's silent hook fail-open. Command guardrails were not enforced for that \
     turn.",
);

/// Register both guard-trace counters with `registry`. Called from
/// [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &crate::metrics::Registry) {
    registry.register_counter(&CODEX_GUARD_TRACE_REPORTED);
    registry.register_counter(&CODEX_GUARDS_SILENT);
}

/// What a guard-trace notification said, once classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardTraceSignal {
    /// Guards ran. Carries the rendered summary (counts plus notable blocks).
    Reported(String),
    /// Tool calls ran with no guard invocation recorded.
    Silent(String),
}

/// Classify one `WorkerEvent::Notification` message.
///
/// Returns `None` for every message that is not a guard-trace notification —
/// in particular every hook-shaped event Claude and the other drivers emit.
pub fn classify(message: &str) -> Option<GuardTraceSignal> {
    if let Some(detail) = message.strip_prefix(boss_engine_driver::codex::GUARDS_SILENT_MARKER) {
        return Some(GuardTraceSignal::Silent(detail.trim().to_owned()));
    }
    if let Some(detail) = message.strip_prefix(boss_engine_driver::codex::GUARD_TRACE_MARKER) {
        return Some(GuardTraceSignal::Reported(detail.trim().to_owned()));
    }
    None
}

/// Record one classified signal: engine log line plus counter.
pub fn record(registry: &crate::metrics::Registry, run_id: Option<&str>, signal: &GuardTraceSignal) {
    match signal {
        GuardTraceSignal::Reported(detail) => {
            CODEX_GUARD_TRACE_REPORTED.inc(registry);
            tracing::info!(run_id, detail = %detail, "codex: PreToolUse guard activity for this turn");
        }
        GuardTraceSignal::Silent(detail) => {
            CODEX_GUARDS_SILENT.inc(registry);
            tracing::error!(
                run_id,
                detail = %detail,
                "codex: tool calls ran with no PreToolUse guard invocation recorded; command \
                 guardrails were not enforced for this turn"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_engine_driver::codex::{GUARD_TRACE_MARKER, GUARDS_SILENT_MARKER};

    #[test]
    fn classifies_a_guard_activity_report() {
        let message = format!("{GUARD_TRACE_MARKER} 3 guard invocation(s): 2 approved, 1 blocked, 0 guard error(s)");
        let signal = classify(&message).expect("must classify");
        assert_eq!(
            signal,
            GuardTraceSignal::Reported("3 guard invocation(s): 2 approved, 1 blocked, 0 guard error(s)".to_owned())
        );
    }

    #[test]
    fn classifies_the_silent_guard_signal() {
        let message = format!("{GUARDS_SILENT_MARKER} 4 tool call(s) ran this turn with no PreToolUse guard");
        match classify(&message) {
            Some(GuardTraceSignal::Silent(detail)) => assert!(detail.starts_with("4 tool call(s)"), "{detail}"),
            other => panic!("expected the silent signal, got {other:?}"),
        }
    }

    #[test]
    fn ignores_unrelated_notifications() {
        assert!(classify("turn aborted: interrupted").is_none());
        assert!(classify("[codex-unobserved-command] sleep 999").is_none());
        assert!(classify("").is_none());
    }
}
