//! Grok/xAI error classification for [`crate::AgentDriver::classify_error`]
//! (design T-13). Must not route through
//! `boss_engine_transient_error::classify_claude_error` — Grok's error
//! taxonomy is xAI-owned and, per its own bundled docs, only partially
//! overlaps with Claude Code's.
//!
//! ## Ground truth
//!
//! Grok's bundled CLI documentation (`~/.grok/docs/user-guide/10-hooks.md`,
//! shipped alongside the `grok` binary) documents the exact classification
//! vocabulary its `StopFailure` hook emits:
//!
//! > the emitted set is Claude Code's vocabulary — `rate_limit`,
//! > `authentication_failed`, `invalid_request`, `server_error`,
//! > `max_output_tokens`, `unknown`. grok emits a subset: capacity errors
//! > (503/529) fold into `rate_limit` as in Claude, and `billing_error` is
//! > never emitted (no signal).
//!
//! `StopFailure` itself is not a channel this classifier reads from — it is
//! documented but never observed firing in practice, so it must not be
//! relied on (see the turn-end-recovery module for the same caution applied
//! to `events.jsonl`). The categories above are still the right vocabulary
//! for classifying whatever raw error text Grok's own error rendering
//! surfaces elsewhere (the transcript, a rendered error line), because they
//! are xAI's own documented taxonomy for exactly this failure population —
//! not a guess imported from Claude's classifier.
//!
//! ## Classification rules
//!
//! - [`WorkerErrorClass::Transient`] — `rate_limit` / `server_error`
//!   (Grok folds 503/529 into `rate_limit`, matching Claude's treatment of
//!   overload as retryable), plus generic transport-level failures
//!   (connection reset, timeout, DNS) that are provider-agnostic.
//! - [`WorkerErrorClass::Permanent`] — `authentication_failed`,
//!   `invalid_request`, `max_output_tokens` (a deterministic limit on the
//!   same request; retrying reproduces it, unlike a transient capacity
//!   error). No `billing_error` markers: Grok's own docs say it never emits
//!   one, so matching for it here would be speculative.
//! - [`WorkerErrorClass::Indeterminate`] — anything else, including Grok's
//!   own `unknown` bucket. Gets the same bounded retry budget as
//!   `Transient` (see `boss_engine_transient_error::RecoveryPolicy`).

use crate::WorkerErrorClass;

/// Substrings (matched against a lowercased, space-padded haystack) marking
/// a **permanent**, non-retryable failure: xAI's `authentication_failed`,
/// `invalid_request`, and `max_output_tokens` `StopFailure` classes, plus
/// their common HTTP/prose renderings.
const PERMANENT_MARKERS: &[&str] = &[
    "authentication_failed",
    "authentication failed",
    "invalid api key",
    "invalid x-api-key",
    "x-api-key",
    "unauthorized",
    " 401 ",
    "http 401",
    "error code: 401",
    "invalid_request",
    "invalid request",
    " 400 ",
    " 404 ",
    "http 400",
    "http 404",
    "error code: 400",
    "error code: 404",
    "model not found",
    "max_output_tokens",
    "maximum output tokens",
    "max output tokens",
    "context window",
    "context_length_exceeded",
    "prompt is too long",
];

/// Substrings marking a **transient**, retryable failure: xAI's
/// `rate_limit` / `server_error` `StopFailure` classes (503/529 fold into
/// `rate_limit` per Grok's own docs), plus generic transport-level errors.
const TRANSIENT_MARKERS: &[&str] = &[
    "rate_limit",
    "rate limit",
    "too many requests",
    "server_error",
    " 429 ",
    " 500 ",
    " 502 ",
    " 503 ",
    " 504 ",
    " 529 ",
    "http 429",
    "http 500",
    "http 502",
    "http 503",
    "http 504",
    "http 529",
    "error code: 429",
    "error code: 500",
    "error code: 502",
    "error code: 503",
    "error code: 504",
    "error code: 529",
    "overloaded",
    "internal server error",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    // Transport / connection — provider-agnostic, same signatures Claude's
    // classifier treats as transient sleep/wake or network-blip artifacts.
    "socket connection was closed",
    "socket hang up",
    "connection reset",
    "econnreset",
    "connection refused",
    "econnrefused",
    "unable to connect",
    "network is unreachable",
    "enetunreach",
    "no route to host",
    "ehostunreach",
    "getaddrinfo",
    "enotfound",
    "name resolution",
    "broken pipe",
    "epipe",
    "unexpected eof",
    "stream error",
    "request timed out",
    "timed out",
    "timeout",
    "etimedout",
    "deadline exceeded",
];

/// Classify a raw Grok/xAI error string. See the module docs for the rule
/// set. Case-insensitive, substring-based; the haystack is space-padded so
/// a bare status code like `"500"` at the very start or end still matches
/// `" 500 "`.
pub fn classify_grok_error(text: &str) -> WorkerErrorClass {
    let haystack = format!(" {} ", text.to_lowercase());
    // Permanent wins on overlap: never auto-retry an unambiguous
    // non-retryable failure just because the message also mentions
    // something that would otherwise read as transient.
    if PERMANENT_MARKERS.iter().any(|m| haystack.contains(m)) {
        return WorkerErrorClass::Permanent;
    }
    if TRANSIENT_MARKERS.iter().any(|m| haystack.contains(m)) {
        return WorkerErrorClass::Transient;
    }
    WorkerErrorClass::Indeterminate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_and_server_error_and_folded_capacity_are_transient() {
        for s in [
            "rate_limit: too many requests",
            "HTTP 429 Too Many Requests",
            "server_error: internal server error",
            "Error code: 503 - service unavailable",
            "Error code: 529 - overloaded",
            "502 Bad Gateway",
            "Request timed out after 600s",
            "connection reset by peer (ECONNRESET)",
        ] {
            assert_eq!(
                classify_grok_error(s),
                WorkerErrorClass::Transient,
                "expected transient for: {s}"
            );
        }
    }

    #[test]
    fn network_blip_shapes_are_transient() {
        for s in [
            "Unable to connect to API (ConnectionRefused)",
            "connect ECONNREFUSED 127.0.0.1:443",
            "getaddrinfo ENOTFOUND api.x.ai",
            "Temporary failure in name resolution",
        ] {
            assert_eq!(
                classify_grok_error(s),
                WorkerErrorClass::Transient,
                "expected transient for: {s}"
            );
        }
    }

    #[test]
    fn authentication_invalid_request_and_max_output_tokens_are_permanent() {
        for s in [
            "authentication_failed: invalid x-api-key",
            "Error code: 401 - unauthorized",
            "invalid_request: messages.0 is invalid",
            "Error code: 400 - invalid_request",
            "max_output_tokens: response truncated at configured limit",
            "prompt is too long: 250000 tokens > 200000 maximum",
        ] {
            assert_eq!(
                classify_grok_error(s),
                WorkerErrorClass::Permanent,
                "expected permanent for: {s}"
            );
        }
    }

    #[test]
    fn unknown_text_is_indeterminate() {
        assert_eq!(
            classify_grok_error("something we have never seen"),
            WorkerErrorClass::Indeterminate
        );
        assert_eq!(classify_grok_error(""), WorkerErrorClass::Indeterminate);
        // Grok's own StopFailure "unknown" bucket text should not
        // accidentally match a Permanent/Transient marker.
        assert_eq!(classify_grok_error("unknown error"), WorkerErrorClass::Indeterminate);
    }

    #[test]
    fn permanent_wins_over_transient_on_overlap() {
        assert_eq!(
            classify_grok_error("authentication_failed after request timed out"),
            WorkerErrorClass::Permanent,
        );
    }

    #[test]
    fn does_not_match_speculative_billing_error() {
        // Grok's own docs say billing_error is never emitted (no signal).
        // A message mentioning "billing" with nothing else recognisable
        // must not be misclassified as Permanent by a marker we have no
        // evidence for.
        assert_eq!(
            classify_grok_error("billing question from support"),
            WorkerErrorClass::Indeterminate
        );
    }
}
